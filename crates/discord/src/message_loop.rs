//! Discordゲートウェイのメッセージ処理ループ（Event-Driven v3）。
//!
//! v3の変更点:
//! - Event-Drivenモデル: IncomingMessageとSubtaskCompletedをmpscチャンネルで処理
//! - P0修正: on_response_textコールバックでストリーミング応答を送信
//! - P1修正: 処理をtokio::spawnで非同期化、メインループをブロックしない
//! - P2修正: SubtaskCompleted callbackをLoopEvent送信に変更、イベントループで直列処理
//!
//! v3.1: P2 の「イベントループで直列処理」は廃止。SubtaskCompleted /
//! InteractionResponse の推論をループ内で await すると、その間**全チャンネル・
//! 全エージェント**の受信処理が停止する（サブタスクが report_progress するたびに
//! メインが無応答になる）。現在は全イベントを spawn + セッション単位ロック
//! （`spawn_serialized_on_session`）で処理し、直列化の範囲を同一セッションに限定する。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use opencrab_core::a2ui::UiRenderer;
use opencrab_gateway::DiscordGateway;
use opencrab_gateway::IncomingMessage;

/// 同一(channel, sender)のメッセージをまとめるまでの待機時間。
const DEBOUNCE_DELAY: Duration = Duration::from_secs(2);

/// セッションID → 推論直列化ロック。
///
/// 同一セッションの推論を直列化し、割り込みメッセージによる履歴不整合（同じ内容の
/// 二重回答）を防ぐために使う。
type SessionLocks = Arc<dashmap::DashMap<String, Arc<tokio::sync::Mutex<()>>>>;

use crate::AgentRunner;

/// メッセージループへの内部イベント。
pub enum LoopEvent {
    /// Discordからの新規メッセージ。
    IncomingMessage(IncomingMessage),
    /// サブタスク完了通知（P2対策: tokio::spawnではなくイベントで直列処理）。
    SubtaskCompleted {
        session_id: String,
        agent_id: String,
        subtask_id: String,
        result: String,
        exit_reason: String,
        channel_id: u64,
        channel_id_str: String,
        guild_id: String,
        is_dm: bool,
    },
    /// A2UIインタラクション応答（ボタンクリック or タイムアウト）。
    InteractionResponse {
        interaction_id: String,
        session_id: String,
        agent_id: String,
        channel_id: u64,
        channel_id_str: String,
        guild_id: String,
        response: opencrab_core::a2ui::A2uiUserAction,
        is_dm: bool,
    },
}

/// Discordのsystem promptに埋め込むcontext行を生成する。
///
/// guild_idが非空のときは `[Discord context: guild_id=..., channel_id=...]`、
/// 空（DM）のときは後方互換のため `[Discord context: channel_id=...]` を返す。
fn discord_context_line(guild_id: &str, channel_id: &str) -> String {
    if guild_id.is_empty() {
        format!("[Discord context: channel_id={}]", channel_id)
    } else {
        format!(
            "[Discord context: guild_id={}, channel_id={}]",
            guild_id, channel_id
        )
    }
}

/// Discord セッションID `discord-{agent_id}-{guild_id}-{channel_id}` から
/// `(guild_id, channel_id)` を復元する。DM は guild_id が空文字列。
///
/// agent_id はハイフンを含みうるため**右から**パースする（channel は数値、
/// guild は数値 or 空、という不変条件を利用）。形式が合わない場合は None。
pub(crate) fn parse_discord_session(session_id: &str) -> Option<(String, u64)> {
    // rsplitn は右から: [channel, guild, "discord-{agent_id}"]
    let mut parts = session_id.rsplitn(3, '-');
    let channel_str = parts.next()?;
    let guild = parts.next()?;
    let rest = parts.next()?;
    if !rest.starts_with("discord-") || rest.len() <= "discord-".len() {
        return None;
    }
    let channel_id: u64 = channel_str.parse().ok()?;
    if !guild.is_empty() && !guild.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((guild.to_string(), channel_id))
}

/// Discordメッセージの受信→エージェント処理→応答送信のEvent-Drivenループ。
///
/// バックグラウンドタスクとして`tokio::spawn`から呼ばれることを想定。
/// Create the event channel pair for the discord loop.
///
/// Returns (sender, receiver). The sender should be cloned and given to
/// DiscordGatewayActions (via `with_a2ui`) so it can inject events.
pub fn create_event_channel() -> (
    mpsc::UnboundedSender<LoopEvent>,
    mpsc::UnboundedReceiver<LoopEvent>,
) {
    mpsc::unbounded_channel()
}

pub async fn run_discord_loop<T: AgentRunner>(
    gateway: Arc<DiscordGateway>,
    state: T,
    agent_ids: Vec<String>,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    owner_discord_id: String,
    pending_registry: Option<crate::PendingInteractionRegistry>,
    event_channel: Option<(
        mpsc::UnboundedSender<LoopEvent>,
        mpsc::UnboundedReceiver<LoopEvent>,
    )>,
    // 共有（TOML）ゲートウェイのループなら true: 専用（per-agent）ゲートウェイが
    // **稼働中**のエージェントをメッセージ処理時にスキップする（#40 — 二重処理防止）。
    // 判定は liveness ベースなので、専用側が停止/起動失敗していれば共有側が
    // フォールバックとして処理を続ける。per-agent ゲートウェイ自身のループ
    // （manager.rs）は必ず false（true にすると自分自身を skip してしまう）。
    skip_agents_with_dedicated_gateway: bool,
    // VC 対話が有効なとき Some。エージェント返信を対応する VC で読み上げる。
    voice: Option<std::sync::Arc<crate::voice_session::VoiceSessionManager>>,
) {
    let (event_tx, mut event_rx) = match event_channel {
        Some((tx, rx)) => (tx, rx),
        None => mpsc::unbounded_channel::<LoopEvent>(),
    };

    // Discord受信をイベントに変換するタスク（P1: メインループをブロックしない）
    {
        let gw = gateway.clone();
        let tx = event_tx.clone();
        tokio::spawn(async move {
            loop {
                match gw.recv().await {
                    Ok(msg) => {
                        let _ = tx.send(LoopEvent::IncomingMessage(msg));
                    }
                    Err(e) => {
                        error!("Discord recv error: {e}");
                        break;
                    }
                }
            }
        });
    }

    // A2UIインタラクション受信タスク: gatewayのinteraction channelから受信して処理
    if let Some(ref registry) = pending_registry {
        let gw = gateway.clone();
        let tx = event_tx.clone();
        let registry = registry.clone();
        let renderer_http = gateway.http().clone();
        tokio::spawn(async move {
            loop {
                match gw.recv_interaction().await {
                    Ok(data) => {
                        handle_component_interaction(
                            data,
                            &registry,
                            renderer_http.clone(),
                            tx.clone(),
                        )
                        .await;
                    }
                    Err(e) => {
                        error!("Discord interaction recv error: {e}");
                        break;
                    }
                }
            }
        });
    }

    info!(
        agents = ?agent_ids,
        "Discord event loop v3 started"
    );

    // イベント処理ループ（直列）: P2のDB競合を構造的に解消
    // デバウンス: 同一(channel_id, sender_id)のメッセージを DEBOUNCE_DELAY 分まとめてから処理
    let mut debounce_buffers: HashMap<(String, String), (Vec<IncomingMessage>, Instant)> =
        HashMap::new();

    // セッション単位の推論直列化ロック。
    // 同一セッションへの推論が並行実行されると、1つ目の応答がまだDBに記録されていない
    // 状態で2つ目の会話履歴が構築され、同じ内容を二重回答してしまう。これを防ぐため、
    // 会話履歴の構築・推論・応答ログをセッション単位で直列化する。
    let session_locks: SessionLocks = Arc::new(dashmap::DashMap::new());

    loop {
        // 次にフラッシュすべきバッファのデッドラインを計算
        let next_deadline = debounce_buffers
            .values()
            .map(|(_, deadline)| *deadline)
            .min();

        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Some(LoopEvent::IncomingMessage(msg)) => {
                        let key = debounce_key(&msg);
                        let entry = debounce_buffers
                            .entry(key)
                            .or_insert_with(|| (Vec::new(), Instant::now() + DEBOUNCE_DELAY));
                        entry.0.push(msg);
                        entry.1 = Instant::now() + DEBOUNCE_DELAY; // タイマーリセット
                    }
                    Some(LoopEvent::SubtaskCompleted {
                        session_id,
                        agent_id,
                        subtask_id,
                        result,
                        exit_reason,
                        channel_id,
                        channel_id_str,
                        guild_id,
                        is_dm,
                    }) => {
                        // 推論をイベントループ内で await しない。以前はここでフル推論を
                        // 直列実行していたため、サブタスクの report_progress / 完了のたびに
                        // 全チャンネル・全エージェントの受信処理が推論終了まで止まっていた
                        // （= サブ実行中メインが無応答になる）。同一セッションの直列化は
                        // セッションロックが引き続き担保する。
                        let gateway_c = gateway.clone();
                        let state_c = state.clone();
                        let ga_c = gateway_actions.clone();
                        let voice_c = voice.clone();
                        let sess = session_id.clone();
                        spawn_serialized_on_session(session_locks.clone(), sess, async move {
                            process_subtask_completed(
                                session_id,
                                agent_id,
                                subtask_id,
                                result,
                                exit_reason,
                                channel_id,
                                channel_id_str,
                                guild_id,
                                is_dm,
                                gateway_c,
                                state_c,
                                ga_c,
                                voice_c,
                            )
                            .await;
                        });
                    }
                    Some(LoopEvent::InteractionResponse {
                        interaction_id,
                        session_id,
                        agent_id,
                        channel_id,
                        channel_id_str,
                        guild_id,
                        response,
                        is_dm,
                    }) => {
                        // SubtaskCompleted と同じ理由でループ内では await しない。
                        let gateway_c = gateway.clone();
                        let state_c = state.clone();
                        let ga_c = gateway_actions.clone();
                        let sess = session_id.clone();
                        spawn_serialized_on_session(session_locks.clone(), sess, async move {
                            process_interaction_response(
                                interaction_id,
                                session_id,
                                agent_id,
                                channel_id,
                                channel_id_str,
                                guild_id,
                                response,
                                is_dm,
                                gateway_c,
                                state_c,
                                ga_c,
                            )
                            .await;
                        });
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep_until(next_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600))), if !debounce_buffers.is_empty() => {
                // デッドラインを過ぎたバッファをフラッシュ
            }
        }

        // デバウンス期限が来たバッファをまとめて処理
        let now = Instant::now();
        let expired_keys: Vec<_> = debounce_buffers
            .iter()
            .filter(|(_, (_, deadline))| *deadline <= now)
            .map(|(k, _)| k.clone())
            .collect();

        for key in expired_keys {
            if let Some((messages, _)) = debounce_buffers.remove(&key) {
                let merged = merge_incoming_messages(messages);
                if let Some(merged) = merged {
                    let count = merged
                        .metadata
                        .get("debounce_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(1);
                    if count > 1 {
                        info!(
                            channel = %key.0,
                            sender = %key.1,
                            count = count,
                            "Debounced messages merged"
                        );
                    }
                    process_incoming_message(
                        merged,
                        gateway.clone(),
                        state.clone(),
                        agent_ids.clone(),
                        gateway_actions.clone(),
                        owner_discord_id.clone(),
                        session_locks.clone(),
                        skip_agents_with_dedicated_gateway,
                        voice.clone(),
                    )
                    .await;
                }
            }
        }
    }

    info!("Discord event loop v3 ended");
}

/// 受信メッセージを処理する。
///
/// バリデーション・セッション設定・エージェント処理のスポーンを行い、即座にリターン（P1）。
#[allow(clippy::too_many_arguments)]
async fn process_incoming_message<T: AgentRunner>(
    incoming: IncomingMessage,
    gateway: Arc<DiscordGateway>,
    state: T,
    agent_ids: Vec<String>,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    owner_discord_id: String,
    session_locks: SessionLocks,
    skip_agents_with_dedicated_gateway: bool,
    voice: Option<std::sync::Arc<crate::voice_session::VoiceSessionManager>>,
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
    // ここでリストごと絞るのは、後段の trust 判定（dm_allowed_any / resolve_caller）にも
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

    // DM whitelist check（いずれかのエージェントが信頼していれば通す事前ゲート）
    if is_dm && !state.dm_allowed_any(&incoming.sender.id, &agent_ids, &owner_discord_id) {
        debug!(
            sender = %incoming.sender.id,
            "Ignoring DM from non-whitelisted user"
        );
        return;
    }

    // Channel whitelist check はエージェントごとに行う（agent loop 内）

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

    // 呼び出し元のアイデンティティを決定（owner > trusted_users.permission > Agent）
    let caller = state.resolve_caller(&incoming.sender.id, &agent_ids, &owner_discord_id);

    let discord_message_id = incoming
        .metadata
        .get("discord_message_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // 処理対象として確定したメッセージに付ける 👀 を一度だけ付与するためのフラグ。
    // bot投稿（自bot/他bot）には付けない。自botは受信側で除外済みだが念のためここでも弾く。
    let mut reaction_added = incoming.sender.is_bot;

    for agent_id in &agent_ids {
        // Per-agent channel whitelist check
        if !is_dm {
            if !state.is_channel_whitelisted_for_agent(&channel_id_str, agent_id) {
                debug!(
                    channel = %channel_id_str,
                    agent = %agent_id,
                    "Ignoring message from non-whitelisted channel for agent"
                );
                continue; // skip this agent, not return
            }
        } else {
            // Per-agent DM trust check.
            // 冒頭のDMゲートは「いずれかのエージェントが信頼していれば通す」判定なので、
            // ここで各エージェント個別に信頼を確認しないと、あるエージェントにしか信頼
            // 登録していないユーザーのDMに全エージェントが応答してしまう。
            if !state.dm_allowed(&incoming.sender.id, agent_id, &owner_discord_id) {
                debug!(
                    sender = %incoming.sender.id,
                    agent = %agent_id,
                    "Ignoring DM: sender not trusted for this agent"
                );
                continue; // skip this agent, not return
            }
        }

        // 処理対象として確定したので 👀 を付ける（DM whitelist / channel whitelist 通過後）。
        // 失敗は非致命的。複数エージェントが同一投稿を処理しても一度だけ付与する。
        if !reaction_added {
            add_seen_reaction(&gateway, channel_id, &channel_id_str, &discord_message_id).await;
            reaction_added = true;
        }

        // タイピングインジケーター送信（ホワイトリスト通過後のみ）
        if let Err(e) = gateway.start_typing(channel_id).await {
            warn!("Failed to start typing indicator: {e}");
        }

        let session_id = format!("discord-{}-{}-{}", agent_id, guild_id, channel_id);
        ensure_discord_session(&state, &session_id, &[agent_id.clone()], &incoming);

        // [Peer Review] 返信の自動記録（#58）: このエージェントの active タスクへ
        // score/gaps/summary を決定的に記録する（LLM のプロンプト規約任せにしない）。
        // 送信者が登録済み co_agent で、未回収の依頼がある場合のみ記録される。
        // 追加処理であり、メッセージはこの後通常どおり LLM にも流れる。
        crate::gateway_actions::record_peer_review_reply(
            state.db(),
            agent_id,
            &session_id,
            &incoming.sender.id,
            &incoming.sender.name,
            &text,
        );

        // NOTE: ユーザーメッセージのログと会話履歴の構築は、推論本体とともに
        // セッション単位ロックの内側（spawn 内）で行う。これにより、割り込みメッセージが
        // 直前の推論完了前に走って履歴が不整合になり、同じ内容を二重回答する問題を防ぐ。

        let (base_prompt, agent_name) = state.build_agent_context(agent_id);
        let system_prompt = format!(
            "{}\n\n{}",
            base_prompt,
            discord_context_line(&guild_id, &channel_id_str)
        );

        let on_response_text: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>> = {
            let state_for_cb = state.clone();
            let gateway_for_cb = gateway.clone();
            let channel_id_str_for_cb = channel_id_str.clone();
            let is_dm_for_cb = is_dm;
            let voice_for_cb = voice.clone();
            let agent_id_for_cb = agent_id.clone();
            Some(std::sync::Arc::new(move |text: String| {
                tracing::warn!(
                    channel_id = channel_id,
                    text_len = text.len(),
                    text_preview = %text.chars().take(100).collect::<String>(),
                    "on_response_text callback invoked"
                );
                if text.is_empty() || text.trim() == "NO_REPLY" {
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
                tokio::spawn(async move {
                    tracing::warn!(
                        channel_id = channel_id,
                        text_len = text.len(),
                        "on_response_text: sending to Discord channel"
                    );
                    if let Err(e) = gateway_cb.send_to_channel(channel_id, &text).await {
                        tracing::error!("on_response_text Discord send failed: {e}");
                    } else {
                        tracing::warn!(
                            channel_id = channel_id,
                            "on_response_text: Discord send succeeded"
                        );
                        // VC セッションがこのチャンネルに紐づいていれば読み上げる
                        if let Some(v) = &voice_cb {
                            v.maybe_speak(&channel_id_str_cb, &agent_id_cb, &text);
                        }
                    }
                });
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
        let channel_id_str_spawn = channel_id_str.clone();
        let sender_id_spawn = incoming.sender.id.clone();
        let sender_name_spawn = incoming.sender.name.clone();
        let sender_avatar_spawn = incoming.sender.avatar_url.clone();
        let text_spawn = text.clone();

        spawn_serialized_on_session(session_locks.clone(), session_id.clone(), async move {
            // ユーザーメッセージをDBにログ（ロック内で履歴の一部として確定させる）。
            state_spawn.record_user_message(
                &session_id_spawn,
                &sender_id_spawn,
                &sender_name_spawn,
                sender_avatar_spawn.as_deref(),
                &channel_id_str_spawn,
                &text_spawn,
                &image_urls_spawn,
            );

            // 会話履歴の構築（直前の応答が確定した後に行うことで二重回答を防ぐ）。
            // 失敗しても early-return せず、末尾のロック回収を必ず通す。
            let budget = state_spawn.context_budget_tokens(&agent_id_spawn);
            let conversation = match state_spawn.build_conversation_string(
                &session_id_spawn,
                &agent_id_spawn,
                budget,
            ) {
                Ok(raw) => Some(prepend_runtime_context_discord(
                    &raw,
                    "Discord conversation",
                    &discord_message_id_spawn,
                )),
                Err(e) => {
                    tracing::error!(session_id = %session_id_spawn, agent_id = %agent_id_spawn, "build_conversation_string failed: {e}");
                    None
                }
            };

            if let Some(conversation) = conversation {
                let result = state_spawn
                    .run_agent_response({
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
                        .with_image_urls(image_urls_spawn.clone());
                        if !discord_message_id_spawn.is_empty() {
                            run_req =
                                run_req.with_trigger_message_id(discord_message_id_spawn.clone());
                        }
                        if let Some(cb) = on_response_text {
                            run_req = run_req.with_on_response_text(cb);
                        }
                        run_req
                    })
                    .await;

                handle_agent_response(
                    result,
                    &agent_id_spawn,
                    &session_id_spawn,
                    &channel_id_str_spawn,
                    &state_spawn,
                )
                .await;
            }
        });
    }
}

/// セッション単位ロックの下で `fut` を実行するタスクを spawn する。
///
/// イベントループを推論でブロックしないための共通経路（受信メッセージ = P1、
/// サブタスク完了/進捗・A2UI 応答 = 旧 P2 直列実行の置き換え）:
/// - ループ自体は即座に次のイベントへ進む（全チャンネル・全エージェントが停止しない）
/// - 同一セッションの履歴構築→推論→応答ログはロックで直列化される（割り込み二重回答の防止）
/// - 終了時に待機者がいなければ map からロックエントリを回収する（#39:
///   session_locks はプロセス生存中に単調増加していた）。remove_if は DashMap の
///   shard 書き込みロック下で述語を評価し、entry() も同じ shard ロックを取るため、
///   「strong_count == 1（= map 以外に保持者なし）」の判定と削除の間に新しい clone が
///   割り込むことはない。待機者がいれば残す。
fn spawn_serialized_on_session<F>(session_locks: SessionLocks, session_id: String, fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let sess_lock = session_locks
        .entry(session_id.clone())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    tokio::spawn(async move {
        let guard = sess_lock.lock().await;
        fut.await;
        drop(guard);
        drop(sess_lock);
        session_locks.remove_if(&session_id, |_, lock| Arc::strong_count(lock) == 1);
    });
}

/// エージェント応答結果を処理してDiscordに送信する。
async fn handle_agent_response<T: AgentRunner>(
    result: anyhow::Result<opencrab_core::EngineResult>,
    agent_id: &str,
    session_id: &str,
    channel_id_str: &str,
    state: &T,
) {
    match result {
        Ok(engine_result) if !engine_result.response.is_empty() => {
            if engine_result.response.trim() == "NO_REPLY" {
                debug!(agent_id = %agent_id, "Agent returned NO_REPLY");
                state.record_agent_no_reply(agent_id, session_id);
                return;
            }
            state.record_agent_reply(
                agent_id,
                session_id,
                channel_id_str,
                &engine_result.response,
                crate::DiscordReplyContext::Direct {
                    tool_calls_made: engine_result.tool_calls_made,
                },
            );
        }
        Ok(_) => debug!(agent_id = %agent_id, "Agent produced empty response"),
        Err(e) => error!(agent_id = %agent_id, error = %e, "SkillEngine failed"),
    }
}

/// サブタスク完了イベントを処理する（P2: イベントループで直列実行）。
async fn process_subtask_completed<T: AgentRunner>(
    session_id: String,
    agent_id: String,
    subtask_id: String,
    _result: String,
    exit_reason: String,
    channel_id: u64,
    channel_id_str: String,
    guild_id: String,
    is_dm: bool,
    gateway: Arc<DiscordGateway>,
    state: T,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    voice: Option<std::sync::Arc<crate::voice_session::VoiceSessionManager>>,
) {
    let (base_prompt, agent_name) = state.build_agent_context(&agent_id);

    // Get task description from subtask session
    let task_description = {
        let sub_session_id = format!("subtask-{}", subtask_id);
        state
            .session_theme(&sub_session_id)
            .map(|theme| {
                // theme is "Subtask: {task}", strip the prefix
                theme
                    .strip_prefix("Subtask: ")
                    .unwrap_or(&theme)
                    .to_string()
            })
            .unwrap_or_default()
    };

    let system_prompt = format!(
        "{}\n\n{}\n[subtask_completed: subtask_id={}, task=\"{}\", exit_reason={}]",
        base_prompt,
        discord_context_line(&guild_id, &channel_id_str),
        subtask_id,
        task_description,
        exit_reason
    );
    let conversation_raw = match state.build_conversation_string(
        &session_id,
        &agent_id,
        state.context_budget_tokens(&agent_id),
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(session_id = %session_id, agent_id = %agent_id, "build_conversation_string failed: {e}");
            return;
        }
    };
    let conversation =
        prepend_runtime_context_discord(&conversation_raw, "Discord conversation", "");

    match state
        .run_agent_response(
            opencrab_actions::RunRequest::new(
                &agent_id,
                &agent_name,
                &session_id,
                &system_prompt,
                &conversation,
                "discord",
                opencrab_actions::CallerIdentity::Agent,
            )
            .with_gateway_actions(gateway_actions),
        )
        .await
    {
        Ok(engine_result) if !engine_result.response.is_empty() => {
            if engine_result.response.trim() == "NO_REPLY" {
                state.record_agent_no_reply(&agent_id, &session_id);
                return;
            }
            if !is_dm && !state.is_channel_writable(&channel_id_str) {
                return;
            }
            if let Err(e) = gateway
                .send_to_channel(channel_id, &engine_result.response)
                .await
            {
                error!("Subtask completion Discord send failed: {e}");
            } else if let Some(v) = &voice {
                v.maybe_speak(&channel_id_str, &agent_id, &engine_result.response);
            }
            state.record_agent_reply(
                &agent_id,
                &session_id,
                &channel_id_str,
                &engine_result.response,
                crate::DiscordReplyContext::SubtaskCompleted,
            );
        }
        _ => {}
    }
}

/// Discordコンポーネントインタラクション（ボタンクリック・セレクトメニュー・モーダルSubmit）を処理する。
///
/// PendingInteractionRegistryから該当するインタラクションを検索し、
/// LoopEvent::InteractionResponseとしてイベントループに送信する。
async fn handle_component_interaction(
    data: opencrab_gateway::ComponentInteractionData,
    registry: &crate::PendingInteractionRegistry,
    renderer_http: Arc<serenity::http::Http>,
    event_tx: mpsc::UnboundedSender<LoopEvent>,
) {
    // Parse custom_id format: "interaction:{uuid}:{component_id}:{action_name}"
    let parts: Vec<&str> = data.custom_id.splitn(4, ':').collect();
    if parts.len() < 4 || parts[0] != "interaction" {
        warn!(custom_id = %data.custom_id, "Invalid A2UI custom_id format");
        return;
    }
    let interaction_id = parts[1].to_string();
    let component_id = parts[2].to_string();
    let action_name = parts[3].to_string();
    // serenityのインタラクション由来のguild_id（DMの場合は空）を保持。
    let guild_id = data.guild_id.clone();

    // Look up in registry, capture fields, then drop the ref
    let pending_data = {
        let pending_ref = registry.get(&interaction_id);
        match pending_ref {
            Some(ref pending) => {
                // Owner-only check
                if !pending.owner_discord_id.is_empty() && data.user_id != pending.owner_discord_id
                {
                    debug!(
                        user_id = %data.user_id,
                        owner_id = %pending.owner_discord_id,
                        "Non-owner tried to interact with owner-only UI"
                    );
                    return;
                }

                Some((
                    pending.session_id.clone(),
                    pending.agent_id.clone(),
                    pending.channel_id,
                    pending.channel_id_str.clone(),
                    pending.is_dm,
                    pending.surface_id.clone(),
                    pending.rendered_message.clone(),
                ))
            }
            None => {
                debug!(
                    interaction_id = %interaction_id,
                    "Interaction not found in registry (expired or already handled)"
                );
                None
            }
        }
    };

    let (session_id, agent_id, channel_id, channel_id_str, is_dm, surface_id, rendered_message) =
        match pending_data {
            Some(d) => d,
            None => return,
        };

    // Handle ModalSubmit: extract field values and merge into context
    if data.interaction_kind == opencrab_gateway::InteractionKind::ModalSubmit {
        // Remove from registry
        let _ = registry.remove(&interaction_id);

        // Build context from modal values
        let mut context = serde_json::Map::new();
        if let Some(modal_values) = &data.modal_values {
            for (field_id, value) in modal_values {
                context.insert(field_id.clone(), serde_json::Value::String(value.clone()));
            }
        }

        let _ = event_tx.send(LoopEvent::InteractionResponse {
            interaction_id,
            session_id,
            agent_id,
            channel_id,
            channel_id_str,
            guild_id: guild_id.clone(),
            response: opencrab_core::a2ui::A2uiUserAction {
                surface_id,
                component_id,
                action_name,
                context: Some(serde_json::Value::Object(context)),
                responder_id: data.user_id,
            },
            is_dm,
        });
        return;
    }

    // Handle SelectMenu: merge selected_values into context
    if data.interaction_kind == opencrab_gateway::InteractionKind::SelectMenu {
        // Remove from registry
        let _ = registry.remove(&interaction_id);

        // Disable the select menu
        let renderer = crate::renderer::DiscordRenderer::new(renderer_http);
        let _ = renderer
            .update_on_response(
                &rendered_message,
                &opencrab_core::a2ui::UserActionResponse {
                    action_name: action_name.clone(),
                    context: None,
                    user_id: data.user_id.clone(),
                },
            )
            .await;

        // Build context with selected_values
        let mut context = serde_json::Map::new();
        if let Some(values) = &data.selected_values {
            context.insert(
                "selected_values".to_string(),
                serde_json::Value::Array(
                    values
                        .iter()
                        .map(|v| serde_json::Value::String(v.clone()))
                        .collect(),
                ),
            );
        }

        let _ = event_tx.send(LoopEvent::InteractionResponse {
            interaction_id,
            session_id,
            agent_id,
            channel_id,
            channel_id_str,
            guild_id: guild_id.clone(),
            response: opencrab_core::a2ui::A2uiUserAction {
                surface_id,
                component_id,
                action_name,
                context: Some(serde_json::Value::Object(context)),
                responder_id: data.user_id,
            },
            is_dm,
        });
        return;
    }

    // Handle Button: Form オープンは gateway の interaction_create で Modal 応答済み（ここには来ない）。

    // Remove from registry
    let _ = registry.remove(&interaction_id);

    // Disable buttons on the message
    let renderer = crate::renderer::DiscordRenderer::new(renderer_http);
    let _ = renderer
        .update_on_response(
            &rendered_message,
            &opencrab_core::a2ui::UserActionResponse {
                action_name: action_name.clone(),
                context: None,
                user_id: data.user_id.clone(),
            },
        )
        .await;

    // Send event to the loop
    let _ = event_tx.send(LoopEvent::InteractionResponse {
        interaction_id,
        session_id,
        agent_id,
        channel_id,
        channel_id_str,
        guild_id,
        response: opencrab_core::a2ui::A2uiUserAction {
            surface_id,
            component_id,
            action_name,
            context: None,
            responder_id: data.user_id,
        },
        is_dm,
    });
}

/// A2UIインタラクション応答イベントを処理する。
///
/// SubtaskCompletedと同様のパターンで、応答情報をシステムプロンプトに含めて
/// エージェントを再呼び出しする。
async fn process_interaction_response<T: AgentRunner>(
    interaction_id: String,
    session_id: String,
    agent_id: String,
    channel_id: u64,
    channel_id_str: String,
    guild_id: String,
    response: opencrab_core::a2ui::A2uiUserAction,
    is_dm: bool,
    gateway: Arc<DiscordGateway>,
    state: T,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
) {
    info!(
        interaction_id = %interaction_id,
        action = %response.action_name,
        component = %response.component_id,
        "Processing A2UI interaction response"
    );

    // 1. Update DB
    {
        let response_json = serde_json::to_string(&response).ok();
        state.mark_interaction_status(
            &interaction_id,
            if response.action_name == "timeout" {
                "timeout"
            } else {
                "responded"
            },
            response_json.as_deref(),
            Some(&response.responder_id),
        );
    }

    // 2. Record in session_log
    {
        let log_content = format!(
            "[interaction_response] ユーザーがUIに応答しました。\nsurface_id: {}\ncomponent_id: {}\naction: {}\ncontext: {}\nresponder: {}",
            response.surface_id,
            response.component_id,
            response.action_name,
            response.context.as_ref().map(|c| c.to_string()).unwrap_or_default(),
            response.responder_id,
        );
        state.record_interaction_response(
            &agent_id,
            &session_id,
            crate::InteractionRecord {
                interaction_id: &interaction_id,
                surface_id: &response.surface_id,
                action_name: &response.action_name,
                component_id: &response.component_id,
                responder_id: &response.responder_id,
                content: &log_content,
            },
        );
    }

    // 3. Re-invoke agent (same pattern as SubtaskCompleted)
    let (base_prompt, agent_name) = state.build_agent_context(&agent_id);

    let context_str = response
        .context
        .as_ref()
        .map(|c| c.to_string())
        .unwrap_or_default();
    let system_prompt = format!(
        "{}\n\n{}\n[interaction_response: interaction_id={}, surface_id={}, action={}, component_id={}, context={}, responder={}]",
        base_prompt,
        discord_context_line(&guild_id, &channel_id_str),
        interaction_id, response.surface_id,
        response.action_name, response.component_id, context_str, response.responder_id,
    );
    let conversation_raw = match state.build_conversation_string(
        &session_id,
        &agent_id,
        state.context_budget_tokens(&agent_id),
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(session_id = %session_id, agent_id = %agent_id, "build_conversation_string failed: {e}");
            return;
        }
    };
    let conversation =
        prepend_runtime_context_discord(&conversation_raw, "Discord conversation", "");

    match state
        .run_agent_response(
            opencrab_actions::RunRequest::new(
                &agent_id,
                &agent_name,
                &session_id,
                &system_prompt,
                &conversation,
                "discord",
                opencrab_actions::CallerIdentity::Agent,
            )
            .with_gateway_actions(gateway_actions),
        )
        .await
    {
        Ok(engine_result) if !engine_result.response.is_empty() => {
            if engine_result.response.trim() == "NO_REPLY" {
                state.record_agent_no_reply(&agent_id, &session_id);
                return;
            }
            if !is_dm && !state.is_channel_writable(&channel_id_str) {
                return;
            }
            if let Err(e) = gateway
                .send_to_channel(channel_id, &engine_result.response)
                .await
            {
                error!("Interaction response Discord send failed: {e}");
            }
            state.record_agent_reply(
                &agent_id,
                &session_id,
                &channel_id_str,
                &engine_result.response,
                crate::DiscordReplyContext::InteractionResponse {
                    interaction_id: &interaction_id,
                },
            );
        }
        _ => {}
    }
}

/// IncomingMessage からセッション用のリッチメタデータとテーマを構築する。
fn build_discord_session_metadata(incoming: &IncomingMessage) -> (String, String) {
    let (guild_id, channel_id) = match &incoming.source {
        opencrab_gateway::MessageSource::Discord {
            guild_id,
            channel_id,
        } => (guild_id.clone(), channel_id.clone()),
        _ => (String::new(), String::new()),
    };

    let is_dm = guild_id.is_empty();

    if is_dm {
        let dm_user_name = incoming.sender.name.clone();
        let theme = format!("DM with {}", dm_user_name);
        let mut meta = serde_json::json!({
            "source": "discord",
            "is_dm": true,
            "channel_id": channel_id,
            "dm_user_name": dm_user_name,
            "dm_user_id": incoming.sender.id,
        });
        if let Some(ref avatar_url) = incoming.sender.avatar_url {
            meta["dm_user_avatar_url"] = serde_json::json!(avatar_url);
        }
        (theme, meta.to_string())
    } else {
        let guild_name = incoming
            .metadata
            .get("guild_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let guild_icon_url = incoming
            .metadata
            .get("guild_icon_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let channel_name = incoming
            .metadata
            .get("channel_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let theme = if !channel_name.is_empty() && !guild_name.is_empty() {
            format!("#{} in {}", channel_name, guild_name)
        } else {
            "Discord conversation".to_string()
        };

        let meta = serde_json::json!({
            "source": "discord",
            "is_dm": false,
            "guild_id": guild_id,
            "guild_name": guild_name,
            "guild_icon_url": guild_icon_url,
            "channel_id": channel_id,
            "channel_name": channel_name,
        });
        (theme, meta.to_string())
    }
}

/// Discordチャンネル用のセッションが存在しなければ作成する。
/// theme/metadata の組み立ては discord 固有、永続化は AgentRunner の意図メソッド。
fn ensure_discord_session<T: AgentRunner>(
    state: &T,
    session_id: &str,
    agent_ids: &[String],
    incoming: &IncomingMessage,
) {
    let (theme, metadata_json) = build_discord_session_metadata(incoming);
    state.ensure_session(session_id, agent_ids, &theme, &metadata_json);
}

/// Discord用: message_idを含む変動コンテキストを前置するヘルパー。
fn prepend_runtime_context_discord(
    user_message: &str,
    session_theme: &str,
    message_id: &str,
) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z");
    let tz_name = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string());
    let now = format!("{now} ({tz_name})");
    format!(
        "[Context]\nCurrent date and time: {now}\nCurrent discussion topic: {session_theme}\nDiscord message_id: {message_id}\n\n{user_message}"
    )
}

/// 処理対象として確定したユーザー投稿に 👀 リアクションを付ける（非致命的）。
///
/// 失敗（権限不足・削除済みメッセージ・無効なID等）してもエラーは握りつぶし、
/// channel_id/message_id とエラー内容のみログに残す（秘密値は含めない）。
async fn add_seen_reaction(
    gateway: &DiscordGateway,
    channel_id: u64,
    channel_id_str: &str,
    message_id: &str,
) {
    const SEEN_EMOJI: &str = "👀";

    let msg_id = match parse_seen_message_id(message_id) {
        Some(id) => id,
        None => {
            if !message_id.is_empty() {
                warn!(
                    channel_id = %channel_id_str,
                    message_id = %message_id,
                    "Skip 👀 reaction: invalid message_id"
                );
            }
            return;
        }
    };
    if let Err(e) = gateway.add_reaction(channel_id, msg_id, SEEN_EMOJI).await {
        warn!(
            channel_id = %channel_id_str,
            message_id = %message_id,
            error = %e,
            "Failed to add 👀 reaction (non-fatal)"
        );
    }
}

/// 👀 リアクションを付ける対象の message_id を解析する。
///
/// 空文字（message_idがメタデータに無い）や数値でない場合は `None` を返し、
/// 呼び出し側はリアクション付与をスキップする。
fn parse_seen_message_id(message_id: &str) -> Option<u64> {
    if message_id.is_empty() {
        return None;
    }
    message_id.parse::<u64>().ok()
}

/// デバウンスキーを生成する: (channel_id, sender_id)。
fn debounce_key(msg: &IncomingMessage) -> (String, String) {
    let channel_id = match &msg.source {
        opencrab_gateway::MessageSource::Discord { channel_id, .. } => channel_id.clone(),
        _ => String::new(),
    };
    (channel_id, msg.sender.id.clone())
}

/// 複数のIncomingMessageを1つにマージする。
/// テキストは改行で結合、画像URLはすべて集約、メタデータは最後のメッセージを使用。
fn merge_incoming_messages(mut messages: Vec<IncomingMessage>) -> Option<IncomingMessage> {
    if messages.is_empty() {
        return None;
    }
    if messages.len() == 1 {
        return Some(messages.remove(0));
    }

    let count = messages.len();
    let mut texts = Vec::new();
    let mut images = Vec::new();

    for msg in &messages {
        let (text, img_urls) = extract_discord_content(&msg.content);
        if !text.is_empty() {
            texts.push(text);
        }
        images.extend(img_urls);
    }

    // 最後のメッセージをベースにする（最新のメタデータ・タイムスタンプ）
    let mut merged = messages.pop().unwrap();

    // コンテンツをマージ
    let merged_text = texts.join("\n");
    if images.is_empty() {
        merged.content = opencrab_gateway::MessageContent::Text(merged_text);
    } else {
        let mut parts: Vec<opencrab_gateway::ContentPart> = Vec::new();
        if !merged_text.is_empty() {
            parts.push(opencrab_gateway::ContentPart::Text(merged_text));
        }
        for url in images {
            parts.push(opencrab_gateway::ContentPart::Image { url, alt: None });
        }
        merged.content = opencrab_gateway::MessageContent::Multi(parts);
    }

    // デバウンスでまとめたことをメタデータに記録
    merged
        .metadata
        .insert("debounce_count".to_string(), serde_json::json!(count));

    Some(merged)
}

/// メッセージコンテンツからテキストと画像URLを抽出する。
fn extract_discord_content(content: &opencrab_gateway::MessageContent) -> (String, Vec<String>) {
    match content {
        opencrab_gateway::MessageContent::Text(t) => (t.clone(), vec![]),
        opencrab_gateway::MessageContent::Image { url, .. } => (String::new(), vec![url.clone()]),
        opencrab_gateway::MessageContent::Multi(parts) => {
            let mut texts = Vec::new();
            let mut urls = Vec::new();
            for part in parts {
                match part {
                    opencrab_gateway::ContentPart::Text(t) => texts.push(t.clone()),
                    opencrab_gateway::ContentPart::Image { url, .. } => urls.push(url.clone()),
                }
            }
            (texts.join(" "), urls)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        discord_context_line, parse_discord_session, parse_seen_message_id,
        spawn_serialized_on_session, SessionLocks,
    };
    use std::sync::Arc;

    #[test]
    fn parse_discord_session_guild_channel() {
        assert_eq!(
            parse_discord_session("discord-crab-111-222"),
            Some(("111".to_string(), 222))
        );
    }

    #[test]
    fn parse_discord_session_dm_has_empty_guild() {
        assert_eq!(
            parse_discord_session("discord-crab--222"),
            Some((String::new(), 222))
        );
    }

    #[test]
    fn parse_discord_session_agent_id_with_hyphens() {
        // agent_id はハイフンを含みうる → 右からのパースで channel/guild を確定する
        assert_eq!(
            parse_discord_session("discord-my-cool-agent-987-654"),
            Some(("987".to_string(), 654))
        );
    }

    #[test]
    fn parse_discord_session_rejects_invalid() {
        // channel が数値でない
        assert_eq!(parse_discord_session("discord-crab-111-abc"), None);
        // guild が数値でも空でもない（agent_id 末尾との混同を防ぐ）
        assert_eq!(parse_discord_session("discord-crab-xyz-222"), None);
        // discord- プレフィックスが無い / セグメント不足
        assert_eq!(parse_discord_session("subtask-1234"), None);
        assert_eq!(parse_discord_session("discord--222"), None);
        assert_eq!(parse_discord_session(""), None);
    }

    #[test]
    fn context_line_includes_guild_id_when_present() {
        assert_eq!(
            discord_context_line("123", "456"),
            "[Discord context: guild_id=123, channel_id=456]"
        );
    }

    #[test]
    fn context_line_omits_guild_id_for_dm() {
        assert_eq!(
            discord_context_line("", "456"),
            "[Discord context: channel_id=456]"
        );
    }

    #[test]
    fn parse_seen_message_id_accepts_valid_numeric_id() {
        assert_eq!(
            parse_seen_message_id("1234567890123456789"),
            Some(1234567890123456789)
        );
    }

    #[test]
    fn parse_seen_message_id_rejects_empty() {
        // メタデータに discord_message_id が無いケース → スキップ
        assert_eq!(parse_seen_message_id(""), None);
    }

    #[test]
    fn parse_seen_message_id_rejects_non_numeric() {
        assert_eq!(parse_seen_message_id("not-a-number"), None);
        assert_eq!(parse_seen_message_id("123abc"), None);
    }

    #[tokio::test]
    async fn test_spawn_serialized_on_session_does_not_block_caller() {
        let locks: SessionLocks = Arc::new(dashmap::DashMap::new());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        // 完了までブロックする future を渡しても、spawn 呼び出し自体は即座に返る
        spawn_serialized_on_session(locks.clone(), "s1".to_string(), async move {
            let _ = rx.await;
        });
        // caller 側はブロックされていない（この行に到達できることが検証）
        let _ = tx.send(());
        // 完了後にロックエントリが回収される
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while locks.contains_key("s1") {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("lock entry must be reclaimed after completion");
    }

    #[tokio::test]
    async fn test_spawn_serialized_same_session_serializes() {
        let locks: SessionLocks = Arc::new(dashmap::DashMap::new());
        let concurrent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..5 {
            let c = concurrent.clone();
            let m = max_seen.clone();
            let d = done.clone();
            spawn_serialized_on_session(locks.clone(), "same".to_string(), async move {
                let now = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                m.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                c.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                d.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while done.load(std::sync::atomic::Ordering::SeqCst) < 5 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("all serialized tasks must finish");
        assert_eq!(
            max_seen.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "same-session futures must never run concurrently"
        );
    }

    #[tokio::test]
    async fn test_spawn_serialized_different_sessions_run_concurrently() {
        let locks: SessionLocks = Arc::new(dashmap::DashMap::new());
        // s-block は保持したまま、s-free が完了できることを確認する
        // （旧実装 = イベントループ直列 await では s-block が全体を塞いでいた）。
        let (hold_tx, hold_rx) = tokio::sync::oneshot::channel::<()>();
        spawn_serialized_on_session(locks.clone(), "s-block".to_string(), async move {
            let _ = hold_rx.await;
        });
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        spawn_serialized_on_session(locks.clone(), "s-free".to_string(), async move {
            let _ = done_tx.send(());
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), done_rx)
            .await
            .expect("other sessions must not be blocked by a long-running session")
            .unwrap();
        let _ = hold_tx.send(());
    }
}
