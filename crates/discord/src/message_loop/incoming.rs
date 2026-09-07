use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::gateway::DiscordGateway;
use opencrab_actions::{
    accept_inbound, delivery_effect, prepare_session_inbound, start_session_turn, AdmittedInbound,
    InboundAgentDrop, InboundLookups, InboundMessageDrop, InboundWork, NormalizedInbound,
    NormalizedInboundEvent, SessionLocks, TranscriptSource,
};
use opencrab_gateway::IncomingMessage;

use crate::AgentRunner;

use super::reactions::{
    add_reaction_non_fatal, build_discord_session_metadata, end_of_speech_qualifies,
    extract_discord_content, prepend_runtime_context_discord, SEEN_EMOJI, SPOKE_EMOJI,
};
use super::turn_completion::handle_agent_response;
use super::{
    discord_context_line, should_emit_drop_log, LoopEvent, V3LivenessProbe, DROP_LOG_LAST,
    DROP_LOG_THROTTLE,
};

/// 受信メッセージを処理する。
///
/// バリデーション・セッション設定・エージェント処理のスポーンを行い、即座にリターン（P1）。
#[allow(clippy::too_many_arguments)]
pub(super) async fn process_incoming_message<T: AgentRunner>(
    incoming: IncomingMessage,
    gateway: Arc<DiscordGateway>,
    state: T,
    agent_ids: Vec<String>,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    owner_discord_id: String,
    session_locks: Arc<SessionLocks>,
    skip_agents_with_dedicated_gateway: bool,
    v3_liveness: Option<V3LivenessProbe>,
    voice: Option<std::sync::Arc<crate::voice_session::VoiceSessionManager>>,
    event_tx: mpsc::UnboundedSender<LoopEvent>,
    subtask_registry: opencrab_actions::subtask::SubtaskRegistry,
    // #543: true なら記録までで終え、推論（run）は起こさない。デバウンス窓で
    // 合流したメッセージのうち、run トリガーでないものに使う。各メッセージを正しい送信者で
    // 個別に会話ログへ残しつつ、run は窓につき 1 回だけにするための分岐。
    // 👀 は record_only では付けない。ターン文脈に含まれたとき（`mark_seen`）に付ける。
    record_only: bool,
    mark_seen: bool,
    preplanned: Option<AdmittedInbound>,
) {
    let (text, image_urls) = extract_discord_content(&incoming.content);
    if text.is_empty() && image_urls.is_empty() {
        return;
    }

    let (guild_id, channel_id_str) = match &incoming.source {
        opencrab_gateway::MessageSource::Discord {
            guild_id,
            channel_id,
        } => (guild_id.clone(), channel_id.clone()),
        _ => return,
    };

    let channel_id: u64 = match channel_id_str.parse() {
        Ok(id) => id,
        Err(_) => return,
    };

    let is_dm = guild_id.is_empty();

    // #40: 専用（per-agent）ゲートウェイが稼働中のエージェントは共有ループでは処理しない。
    // ここでリストごと絞るのは、後段の core inbound（accept_inbound）にも
    // スキップ対象エージェントの trusted_users を混入させないため。専用ゲートウェイが
    // 停止/起動失敗していれば絞られず、共有側がフォールバックとして処理を続ける。
    let agent_ids: Vec<String> = if skip_agents_with_dedicated_gateway {
        let filtered: Vec<String> = agent_ids
            .into_iter()
            .filter(|agent_id| {
                if state.served_by_dedicated_gateway(agent_id) {
                    debug!(
                        agent = %agent_id,
                        "Skipping agent on shared gateway: dedicated per-agent gateway is running"
                    );
                    false
                } else {
                    true
                }
            })
            .collect();
        if filtered.is_empty() {
            return;
        }
        filtered
    } else {
        agent_ids
    };

    // DESIGN-DISCORD-GATE §8.1: per-agent（legacy）ループは、同じ agent を V3 gateway process が
    // **実際に受信中**なら退く（二重受信防止）。これが無いと legacy 車線が同一メッセージを
    // 二重処理し、V3 が正しい返信を出す横で 👀→NO_REPLY→🤐 を付ける（本バグの症状）。
    // 判定は probe（core の live registry 由来）で行い、DB の enabled ではない。probe が false
    // （V3 死亡/未接続/ロック失敗）なら退かず legacy が処理を続けて外形を減らさない。
    // 共有ループは `v3_liveness=None` で、上の `served_by_dedicated_gateway` が V3 を OR 済み
    // （二重ゲート回避）。ここで agent 単位に絞るのは、#40 と同じく後段 core inbound の
    // trusted_users にスキップ対象を混ぜないため。
    let agent_ids: Vec<String> = if let Some(ref probe) = v3_liveness {
        let filtered: Vec<String> = agent_ids
            .into_iter()
            .filter(|agent_id| {
                if probe(agent_id) {
                    debug!(
                        agent = %agent_id,
                        "Skipping agent on legacy per-agent gateway: live V3 gateway is receiving"
                    );
                    false
                } else {
                    true
                }
            })
            .collect();
        if filtered.is_empty() {
            return;
        }
        filtered
    } else {
        agent_ids
    };

    // 誰か・権限は core の inbound 1 口。flush 経路は束を既に投げ済み。
    let inbound_event = NormalizedInboundEvent {
        sender_id: &incoming.sender.id,
        channel_id: &channel_id_str,
        guild_id: &guild_id,
    };
    let plan = if let Some(pre) = preplanned {
        pre
    } else {
        let work = InboundWork {
            event: inbound_event,
            has_content: true,
            kind_label: "",
            author_key: &incoming.sender.id,
        };
        let mut admitted = None;
        let accept_err = {
            let resolve = |s: &str, a: &[String], o: &str| state.resolve_caller(s, a, o);
            let dm_any = |s: &str, a: &[String], o: &str| state.dm_allowed_any(s, a, o);
            let dm = |s: &str, a: &str, o: &str| state.dm_allowed(s, a, o);
            let wl = |c: &str, a: &str| state.is_channel_whitelisted_for_agent(c, a);
            let lookups = InboundLookups {
                resolve_caller: &resolve,
                dm_allowed_any: &dm_any,
                dm_allowed: &dm,
                channel_whitelisted: &wl,
            };
            accept_inbound::<()>(
                &[work],
                &owner_discord_id,
                &agent_ids,
                &lookups,
                None,
                |_| (),
                |_, adm| admitted = Some(adm.clone()),
                |_, _, _| {},
            )
        };
        match accept_err {
            Ok(()) => admitted.expect("1 件の対話系は通るか Message drop"),
            Err(opencrab_actions::InboundDrop::Message(InboundMessageDrop::DmNotTrusted)) => {
                // #419: 破棄は正しい動作だが debug だと運用ログ（INFO）に出ず「無言・エラー
                // なし」の切り分けが難しい。設定による破棄を 1 行 INFO で残す（宛先ごとに間引き）。
                let key = format!("dm_gate:{}", incoming.sender.id);
                if should_emit_drop_log(&DROP_LOG_LAST, &key, Instant::now(), DROP_LOG_THROTTLE) {
                    info!(
                        sender = %incoming.sender.id,
                        reason = "dm_sender_not_trusted",
                        "受信DMを破棄: 設定によりどのエージェントも送信者を信頼していない"
                    );
                }
                return;
            }
            Err(e) => {
                unreachable!("watch 無しの対話系で Policy は出ない: {e}");
            }
        }
    };

    debug!(
        user = %incoming.sender.name,
        channel = channel_id,
        text = %text.chars().take(50).collect::<String>(),
        "Discord message received"
    );

    if !state.has_llm_providers() {
        debug!("No LLM providers configured, skipping agent response");
        return;
    }

    let caller = plan.caller.clone();

    let discord_message_id = incoming
        .metadata
        .get("discord_message_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // LLM がこの投稿を読んだ（ターン文脈に含めた）ときに付ける 👀 を**一度だけ**
    // 付与するためのフラグ。複数エージェントが同じ投稿を処理しても 1 個で済ませる。
    // 送信者で付け外しはしない（自分自身の投稿は受信側 `is_own_message` で既に除外済み）。
    let mut reaction_added = false;

    for agent_id in &agent_ids {
        match plan.agent_drop(agent_id) {
            None => {}
            Some(InboundAgentDrop::ChannelNotWhitelisted) => {
                // #419: 設定によるチャンネル破棄を 1 行 INFO で残す（宛先ごとに間引き）。
                let key = format!("chan_wl:{agent_id}:{channel_id_str}");
                if should_emit_drop_log(&DROP_LOG_LAST, &key, Instant::now(), DROP_LOG_THROTTLE) {
                    info!(
                        channel = %channel_id_str,
                        agent = %agent_id,
                        reason = "channel_not_whitelisted",
                        "受信メッセージを破棄: 設定によりこのエージェントの非whitelistチャンネル"
                    );
                }
                continue;
            }
            Some(InboundAgentDrop::DmNotTrustedForAgent) => {
                // #419: 設定による DM 破棄を 1 行 INFO で残す（宛先ごとに間引き）。
                let key = format!("dm_trust:{}:{}", agent_id, incoming.sender.id);
                if should_emit_drop_log(&DROP_LOG_LAST, &key, Instant::now(), DROP_LOG_THROTTLE) {
                    info!(
                        sender = %incoming.sender.id,
                        agent = %agent_id,
                        reason = "dm_sender_not_trusted_for_agent",
                        "受信DMを破棄: 設定によりこのエージェントは送信者を信頼していない"
                    );
                }
                continue;
            }
        }

        let session_id = format!("discord-{}-{}-{}", agent_id, guild_id, channel_id);
        let (theme, metadata_json) = build_discord_session_metadata(&incoming);
        let inbound = NormalizedInbound {
            session_id: &session_id,
            agent_id,
            sender_id: &incoming.sender.id,
            sender_name: &incoming.sender.name,
            avatar_url: incoming.sender.avatar_url.as_deref(),
            channel_id: Some(&channel_id_str),
            pubkey: None,
            text: &text,
            image_urls: &image_urls,
            external_id: &discord_message_id,
        };

        // #284 P0-1 / #286: ユーザー発言の記録は**この処理で最初に行う副作用**。
        // セッションロックより前、Discord API より前。確保と記録は core。
        debug!(agent_id = %agent_id, session_id = %session_id, stage = "record_inbound", "turn: 受信記録 開始（入）");
        if !prepare_session_inbound(
            &state,
            TranscriptSource::Discord,
            &inbound,
            &theme,
            &metadata_json,
            "discord",
        ) {
            crate::owner_warning::warn_inbound_message_dropped(
                &session_id,
                &incoming.sender.id,
                text.len(),
            );
        }
        debug!(agent_id = %agent_id, session_id = %session_id, stage = "record_inbound", "turn: 受信記録 完了（出）");

        // LLM がこの投稿を読んだ（ターン文脈に含めた）ので 👀 を付ける。
        // record-only 単体では付けない。whitelist 通過後。失敗は非致命的。
        // 複数エージェントが同一投稿を処理しても一度だけ付与する。
        if mark_seen && !reaction_added {
            add_reaction_non_fatal(
                gateway.as_ref(),
                channel_id,
                &channel_id_str,
                &discord_message_id,
                SEEN_EMOJI,
            )
            .await;
            reaction_added = true;
        }

        // #543: record-only パス（デバウンス窓の非トリガーメッセージ）は記録まで。
        // typing / 推論（run）は起こさない。👀 は上の `mark_seen`（読むターンが走った時）。
        // 合流窓のトリガー 1 通だけが run を起こし、その run が DB から会話全体（この記録も含む）
        // を読むので、情報は落ちず文脈には正しい帰属で入る。
        if record_only {
            continue;
        }

        // タイピングインジケーター（ホワイトリスト通過後のみ）。
        // #429: 1 回だけ打つと Discord の失効（約 10 秒）で応答前に消えるため、ターンが
        // 生きている間は打ち直し続ける keepalive を起こす。ガード `typing_keepalive` は
        // 下の spawn_serialized 内へ move し、ターン終了（成功・空・NO_REPLY・エラー）で
        // drop されて確実に停止する。keepalive は別タスクなのでイベントループもターン本体も
        // ブロックしない。発火条件は従来どおり（ここに来た＝応答する体だけ）。
        let typing_keepalive = {
            let gw = gateway.clone();
            crate::typing_keepalive::spawn_typing_keepalive(
                crate::typing_keepalive::TYPING_REFRESH_INTERVAL,
                move || {
                    let gw = gw.clone();
                    async move {
                        if let Err(e) = gw.start_typing(channel_id).await {
                            warn!("Failed to refresh typing indicator: {e}");
                        }
                    }
                },
            )
        };

        // NOTE: 会話履歴の構築は、推論本体とともにセッション単位ロックの内側（spawn 内）で
        // 行う。これにより、割り込みメッセージが直前の推論完了前に走って履歴が不整合に
        // なり、同じ内容を二重回答する問題を防ぐ。

        // #352: 本ターンの caller（core の inbound 1 口が解決）で index を絞る。
        let (base_prompt, agent_name) = state.build_agent_context(agent_id, &caller);
        let system_prompt = format!(
            "{}\n\n{}",
            base_prompt,
            discord_context_line(&guild_id, &channel_id_str)
        );

        // #431: 「発言終わり」リアクション用に、このターンで自分が最後に投稿した
        // メッセージ id を追跡する。on_response_text は反復ごとに発火しうるため、
        // 送信は detach spawn（P1 非ブロック）のままにしつつ、**発火順（seq）が最大**の
        // 送信を「最後の投稿」として採る（完了順は前後しうるので発火順で選ぶ）。
        // ターン終了時に送信完了を待ってからリアクションを打つため、ハンドルも集める。
        let last_self_post: std::sync::Arc<std::sync::Mutex<(u64, Option<u64>)>> =
            std::sync::Arc::new(std::sync::Mutex::new((0, None)));
        let reply_send_seq = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let reply_send_tasks: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        // #431: このターンが background subtask を起こしたか。`reply_send_seq` と同じ
        // ターン寿命で、run が返った後に読む。自動 dispatch と明示 `spawn_subtask` の
        // 両経路が、登録簿への登録が成立したところで加算する（`RunRequest::subtask_starts`）。
        let subtask_starts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let on_response_text: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>> = {
            let state_for_cb = state.clone();
            let gateway_for_cb = gateway.clone();
            let channel_id_str_for_cb = channel_id_str.clone();
            let is_dm_for_cb = is_dm;
            let voice_for_cb = voice.clone();
            let agent_id_for_cb = agent_id.clone();
            let last_self_post_cb = last_self_post.clone();
            let reply_send_seq_cb = reply_send_seq.clone();
            let reply_send_tasks_cb = reply_send_tasks.clone();
            Some(std::sync::Arc::new(move |text: String| {
                tracing::warn!(
                    channel_id = channel_id,
                    text_len = text.len(),
                    text_preview = %text.chars().take(100).collect::<String>(),
                    "on_response_text callback invoked"
                );
                // 第一柱: NO_REPLY 終端解釈で前段のみ配送（空・単独 NO_REPLY はスキップ）。
                // 破棄ログは最終応答を判定する delivery_effect が出す（反復途中での二重計上を避ける）。
                let text = match opencrab_actions::terminate_at_no_reply(&text).speech() {
                    Some(s) if !s.trim().is_empty() => s.to_string(),
                    _ => return,
                };
                // #890 §11.7: 最終行 CONTINUE 単独を剥がす（継続判定は engine 済み・ここは表示
                // 保護）。WARN は delivery_effect 側へ集約（反復途中での二重計上を避ける）。
                let text = opencrab_actions::strip_trailing_continue(&text)
                    .map(str::to_string)
                    .unwrap_or(text);
                if text.trim().is_empty() {
                    return;
                }
                let writable =
                    is_dm_for_cb || state_for_cb.is_channel_writable(&channel_id_str_for_cb);
                if !writable {
                    tracing::warn!(channel_id_str = %channel_id_str_for_cb, "on_response_text: channel not writable, skipping Discord send");
                    return;
                }
                let gateway_cb = gateway_for_cb.clone();
                let voice_cb = voice_for_cb.clone();
                let channel_id_str_cb = channel_id_str_for_cb.clone();
                let agent_id_cb = agent_id_for_cb.clone();
                let last_self_post_task = last_self_post_cb.clone();
                // 発火順の連番。後段で最大 seq の送信＝最後の投稿を採る。
                let seq = reply_send_seq_cb.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let handle = tokio::spawn(async move {
                    tracing::warn!(
                        channel_id = channel_id,
                        text_len = text.len(),
                        "on_response_text: sending to Discord channel"
                    );
                    match gateway_cb.send_to_channel(channel_id, &text).await {
                        Ok(msg_id) => {
                            tracing::warn!(
                                channel_id = channel_id,
                                "on_response_text: Discord send succeeded"
                            );
                            // #431: 発火順が最大の送信だけを「最後の投稿」として記録する。
                            if let Some(id) = msg_id {
                                let mut g = last_self_post_task.lock().unwrap();
                                if seq >= g.0 {
                                    *g = (seq, Some(id));
                                }
                            }
                            // VC セッションがこのチャンネルに紐づいていれば読み上げる
                            if let Some(v) = &voice_cb {
                                v.maybe_speak(&channel_id_str_cb, &agent_id_cb, &text);
                            }
                        }
                        Err(e) => {
                            tracing::error!("on_response_text Discord send failed: {e}");
                        }
                    }
                });
                reply_send_tasks_cb.lock().unwrap().push(handle);
            }))
        };

        // エージェント処理をバックグラウンドspawnで実行（P1: メインループをブロックしない）。
        // ただしセッション単位ロックで直列化し、履歴の構築→推論→応答ログを不可分にする。
        let state_spawn = state.clone();
        let ga_spawn = gateway_actions.clone();
        let agent_id_spawn = agent_id.clone();
        let agent_name_spawn = agent_name.clone();
        let session_id_spawn = session_id.clone();
        let system_prompt_spawn = system_prompt.clone();
        let caller_spawn = caller.clone();
        let image_urls_spawn = image_urls.clone();
        let discord_message_id_spawn = discord_message_id.clone();
        // NO_REPLY の可視化（#317）で使う。`gateway_for_cb`（:657）と同じ形の持ち込み。
        let gateway_spawn = gateway.clone();
        let channel_id_str_spawn = channel_id_str.clone();
        let sender_id_spawn = incoming.sender.id.clone();
        let sender_name_spawn = incoming.sender.name.clone();
        let sender_avatar_spawn = incoming.sender.avatar_url.clone();
        let text_spawn = text.clone();
        let event_tx_spawn = event_tx.clone();
        let registry_spawn = subtask_registry.clone();
        // #429: typing keepalive をターン本体へ move する。この future がどの経路で
        // 終わっても（下の早期パスを含む）ここで束ねたガードが drop され、keepalive は停止する。
        let typing_keepalive_spawn = typing_keepalive;
        // #431: 「発言終わり」リアクションの判定に使う（最後の自分の投稿 id / 送信ハンドル /
        // 実送信を試みた回数＝このターンで発話したか）。
        let last_self_post_spawn = last_self_post.clone();
        let reply_send_tasks_spawn = reply_send_tasks.clone();
        let reply_send_seq_spawn = reply_send_seq.clone();
        let subtask_starts_spawn = subtask_starts.clone();

        // #665: ターン本体を session 直列キューへ投入する（結果は待たない・#223）。この後、直列ロックの
        // 取得は共通の `SessionLocks::run_serialized`（session_lock 段）で計装される。
        debug!(agent_id = %agent_id, session_id = %session_id, stage = "enqueue_turn", "turn: ターンを直列キューへ投入");
        session_locks.spawn_serialized(session_id.clone(), async move {
            // ターンの寿命に typing keepalive を束ねる（#429）。名前付きで保持し、
            // ブロック終端まで生かす。drop = keepalive 停止。
            let _typing_keepalive = typing_keepalive_spawn;
            // NOTE: ユーザーメッセージの記録はロックより前に済んでいる（#284 P0-1）。
            // ターン起動（フック・文脈・run）は core。配送は下の handle_agent_response。
            let inbound = NormalizedInbound {
                session_id: &session_id_spawn,
                agent_id: &agent_id_spawn,
                sender_id: &sender_id_spawn,
                sender_name: &sender_name_spawn,
                avatar_url: sender_avatar_spawn.as_deref(),
                channel_id: Some(&channel_id_str_spawn),
                pubkey: None,
                text: &text_spawn,
                image_urls: &image_urls_spawn,
                external_id: &discord_message_id_spawn,
            };
            debug!(agent_id = %agent_id_spawn, session_id = %session_id_spawn, stage = "context_build", "turn: 文脈構築 開始（入）");
            if let Some(result) = start_session_turn(
                &state_spawn,
                TranscriptSource::Discord,
                &inbound,
                &system_prompt_spawn,
                // 予算計上は wrap が前置する runtime context と一致させる（同じ theme / message_id）。
                &prepend_runtime_context_discord("", "Discord conversation", &discord_message_id_spawn),
                |raw| {
                    debug!(
                        session_id = %session_id_spawn,
                        agent_id = %agent_id_spawn,
                        conversation_len = raw.len(),
                        stage = "context_build",
                        "turn: 文脈構築 完了（出）"
                    );
                    prepend_runtime_context_discord(
                        raw,
                        "Discord conversation",
                        &discord_message_id_spawn,
                    )
                },
                |conversation| {
                    let mut run_req = opencrab_actions::RunRequest::new(
                        &agent_id_spawn,
                        &agent_name_spawn,
                        &session_id_spawn,
                        &system_prompt_spawn,
                        &conversation,
                        "discord",
                        caller_spawn,
                    )
                    .with_gateway_actions(ga_spawn)
                    .with_reply_target(channel_id_str_spawn.clone())
                    .with_image_urls(image_urls_spawn.clone());
                    if !discord_message_id_spawn.is_empty() {
                        run_req = run_req.with_trigger_message_id(discord_message_id_spawn.clone());
                    }
                    if let Some(cb) = on_response_text {
                        run_req = run_req.with_on_response_text(cb);
                    }
                    let sink: std::sync::Arc<dyn opencrab_actions::SubtaskCompletionSink> =
                        std::sync::Arc::new(crate::gateway_actions::DiscordCompletionSink {
                            event_tx: Some(event_tx_spawn.clone()),
                        });
                    run_req = run_req.with_dispatch(Some(registry_spawn.clone()), sink);
                    run_req = run_req.with_subtask_starts(subtask_starts_spawn.clone());
                    run_req
                },
            )
            .await
            {

                // #431: 「発言終わり」リアクションの可否を effect を move する前に確定する。
                // 「発話したか」は最終応答テキストではなく、このターンで on_response_text が
                // 送信タスクを起こした回数で見る（run_agent_response は既に完了しているので
                // 発火は出揃っている。送信タスク自体の完了待ちは下の detach 側で行う）。
                // 反復途中で喋って最終応答が NO_REPLY のターンを取りこぼさないため。
                let effect = delivery_effect(
                    result,
                    opencrab_actions::DeliveryContext {
                        session_id: &session_id_spawn,
                        agent_id: &agent_id_spawn,
                        origin: "discord",
                    },
                );
                let posted =
                    reply_send_seq_spawn.load(std::sync::atomic::Ordering::SeqCst) > 0;
                // このターンが「次の行動」を起こしたか（自動 dispatch / 明示 spawn_subtask）。
                let started_subtask =
                    subtask_starts_spawn.load(std::sync::atomic::Ordering::SeqCst) > 0;
                let eos_qualifies = end_of_speech_qualifies(&effect, posted, started_subtask);

                // #665: run から戻り、最終応答の処理・配送（記録／NO_REPLY 可視化）へ入る段。反復途中の
                // 配送は on_response_text の detach spawn（別途 warn ログあり）で、ここは最終応答の後始末。
                debug!(agent_id = %agent_id_spawn, session_id = %session_id_spawn, stage = "reply", "turn: 応答処理・配送 開始（入）");
                handle_agent_response(
                    effect,
                    &agent_id_spawn,
                    &session_id_spawn,
                    channel_id,
                    &channel_id_str_spawn,
                    &state_spawn,
                    gateway_spawn.as_ref(),
                    &discord_message_id_spawn,
                )
                .await;
                debug!(agent_id = %agent_id_spawn, session_id = %session_id_spawn, stage = "reply", "turn: 応答処理・配送 完了（出）");

                // #431: 自然終了かつ発話ありなら、そのターンで自分が最後に投稿した
                // メッセージに SPOKE_EMOJI を付ける。ストリーミング送信（detach spawn）が
                // 全て完了してから最後の投稿 id を読むが、その待機はセッションロックを
                // 塞がないよう別 detach タスクで行う（応答経路もブロックしない・non-fatal）。
                if eos_qualifies {
                    let handles: Vec<tokio::task::JoinHandle<()>> =
                        std::mem::take(&mut *reply_send_tasks_spawn.lock().unwrap());
                    let last_self_post_react = last_self_post_spawn.clone();
                    let gateway_react = gateway_spawn.clone();
                    let channel_id_str_react = channel_id_str_spawn.clone();
                    tokio::spawn(async move {
                        for h in handles {
                            let _ = h.await;
                        }
                        let last_id = last_self_post_react.lock().unwrap().1;
                        if let Some(id) = last_id {
                            add_reaction_non_fatal(
                                gateway_react.as_ref(),
                                channel_id,
                                &channel_id_str_react,
                                &id.to_string(),
                                SPOKE_EMOJI,
                            )
                            .await;
                        }
                    });
                }
            }
        });
    }
}
