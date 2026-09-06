use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::gateway::DiscordGateway;
use opencrab_actions::{
    accept_inbound, AdmittedInbound, InboundLookups, InboundMessageDrop, InboundWork,
    NormalizedInboundEvent,
};
use opencrab_gateway::IncomingMessage;

use crate::AgentRunner;

use super::incoming::process_incoming_message;
use super::interactions::{handle_component_interaction, process_interaction_response};
use super::reactions::{debounce_window_key, incoming_has_content};
use super::turn_completion::{process_subtask_completed, process_timed_fire};
use super::{
    recv_retry_backoff, should_alert_inbound_stalled, should_emit_drop_log, LoopEvent,
    V3LivenessProbe, DEBOUNCE_DELAY, DROP_LOG_LAST, DROP_LOG_THROTTLE,
};

// Discord ループの起動エントリ。各引数は独立した依存（gateway / state / registry /
// voice / 各種フラグ）で、構造体化しても呼び出し側の見通しが良くならないため許容する。
#[allow(clippy::too_many_arguments)]
pub async fn run_discord_loop<T: AgentRunner>(
    gateway: Arc<DiscordGateway>,
    state: T,
    agent_ids: Vec<String>,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    owner_discord_id: String,
    pending_registry: Option<opencrab_core::a2ui::PendingInteractionRegistry>,
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
    // per-agent（legacy）ループなら Some: 同じ agent を V3 gateway process が**実際に受信中**
    // のときメッセージ処理をスキップする（DESIGN-DISCORD-GATE §8.1 — 二重受信防止）。判定は
    // core の live registry 由来の probe（`V3LivenessProbe`）で、DB の enabled ではない。
    // 共有（TOML）ループは `served_by_dedicated_gateway`（V3AwareGateway が V3 liveness を OR）で
    // 既に V3 を除外するので、こちらは **None**（この lever は per-agent ループ専用・二重ゲート回避）。
    v3_liveness: Option<V3LivenessProbe>,
    // VC 対話が有効なとき Some。エージェント返信を対応する VC で読み上げる。
    voice: Option<std::sync::Arc<crate::voice_session::VoiceSessionManager>>,
    // auto-dispatch した background subtask を載せる共有 registry（RFC #152 S3a / P0）。
    // `DiscordGatewayActions` と**同一**の registry を渡すことで、auto-dispatch した
    // 単一ツール subtask が `cancel_subtask` の認可ゲート経由で親/owner から停止可能になる。
    subtask_registry: opencrab_actions::subtask::SubtaskRegistry,
) {
    let (event_tx, mut event_rx) = match event_channel {
        Some((tx, rx)) => (tx, rx),
        None => mpsc::unbounded_channel::<LoopEvent>(),
    };

    // Discord受信をイベントに変換するタスク（P1: メインループをブロックしない）
    //
    // #284 P0-2: **このタスクは recv エラーで死んではいけない。**
    // 以前は `recv()` が `Err` を返した時点で `break` していた。以後 `IncomingMessage` は
    // 二度と流れないが、`SubtaskCompleted` 等は別経路で届き続けるため、外からは
    // 「ループは生きているのにユーザーの発言だけが永久に届かない」状態に見える。
    // 抜けてよいのはイベントループ側が畳まれたとき（送信先チャンネルが閉じたとき）だけ。
    //
    // **ただしこれは #284 の真因ではない**（#286 のレビューで判明）。現在の
    // `crate::gateway::DiscordGateway::recv` が `Err` を返すのは
    // 受信チャンネルの全 Sender が drop されたときだけで、その `tx` は `DiscordGateway`
    // 構造体のフィールドとして保持されている。ゲートウェイが生きている限り `Err` は
    // 起きず、旧コードの `break` は**到達不能**だった。真因は別（イベントループの滞留が
    // 有力）。ここに残すのは、実装が変わって `Err` が起きうるようになったときに
    // 「沈黙して死ぬ」形へ戻らないための防御であって、事故の説明ではない。
    {
        let gw = gateway.clone();
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let mut consecutive_failures: u32 = 0;
            let mut last_ok = Instant::now();
            loop {
                match gw.recv().await {
                    Ok(msg) => {
                        consecutive_failures = 0;
                        last_ok = Instant::now();
                        if tx.send(LoopEvent::IncomingMessage(msg)).is_err() {
                            // 受け手（イベントループ）が終了した。ここでだけ抜ける。
                            warn!("Discord event loop receiver closed; stopping inbound forwarder");
                            break;
                        }
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        let backoff = recv_retry_backoff(consecutive_failures);
                        error!(
                            failures = consecutive_failures,
                            secs_since_last_message = last_ok.elapsed().as_secs(),
                            retry_in_ms = backoff.as_millis() as u64,
                            "Discord recv error: {e}"
                        );
                        if should_alert_inbound_stalled(consecutive_failures) {
                            crate::owner_warning::warn_inbound_stalled(
                                consecutive_failures,
                                last_ok.elapsed().as_secs(),
                            );
                        }
                        tokio::time::sleep(backoff).await;
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
        // #337: このタスクも recv エラーで**黙って死んではいけない**（受信転送 #284 と同型）。
        // 以前は `recv_interaction()` が `Err` を返した時点で `break` して黙って終了し、
        // 以後ボタン/セレクト/モーダルの応答が一切届かなくなっていた（誰も気づけない）。
        // 受信転送側と同じく、指数バックオフで再試行しつつ、連続失敗が続いたら
        // オーナーへエスカレーションする。閾値・バックオフは受信転送と同じ実装を共有する。
        tokio::spawn(async move {
            let mut consecutive_failures: u32 = 0;
            let mut last_ok = Instant::now();
            loop {
                match gw.recv_interaction().await {
                    Ok(data) => {
                        consecutive_failures = 0;
                        last_ok = Instant::now();
                        handle_component_interaction(
                            data,
                            &registry,
                            renderer_http.clone(),
                            tx.clone(),
                        )
                        .await;
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        let backoff = recv_retry_backoff(consecutive_failures);
                        error!(
                            failures = consecutive_failures,
                            secs_since_last_ok = last_ok.elapsed().as_secs(),
                            retry_in_ms = backoff.as_millis() as u64,
                            "Discord interaction recv error: {e}"
                        );
                        if should_alert_inbound_stalled(consecutive_failures) {
                            crate::owner_warning::warn_interaction_recv_stalled(
                                consecutive_failures,
                                last_ok.elapsed().as_secs(),
                            );
                        }
                        tokio::time::sleep(backoff).await;
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
    // #543 / #556: デバウンス**バッファ**は **channel ごとに 1 本**（タイマーも 1 本）。
    // オーナー指示「デバウンスはチャンネルごと。人で分けたらだめ」そのまま。全メッセージは
    // 個別に記録して送信者の帰属を保つ。run は**フラッシュ時に切る「連続同権限グループ」**
    // ごとに 1 回（下のフラッシュ箇所を参照）。
    //
    // **権限で並行バッファに割らない理由（#556）**: run は DB から会話全体を読むので、権限で
    // 別バッファに割ると同じ文脈に対して 2 回 run が起き 2 回答える＝減らそうとした増幅になる。
    // かといって channel だけで丸ごと 1 run にすると、caller が最後の送信者で決まり owner 指示が
    // 降格しうる。両方を避けるため、バッファは 1 本にしつつ**フラッシュ時に連続同権限で
    // グループへ切る**（グループ内は権限が揃うので caller が一意・別権限は別 run で混ざらない）。
    let mut debounce_buffers: HashMap<String, (Vec<IncomingMessage>, Instant)> = HashMap::new();

    // セッション単位の推論直列化ランタイム（gateway 非依存層の共通実装 / #156 S2）。
    // 同一セッションへの推論が並行実行されると、1つ目の応答がまだDBに記録されていない
    // 状態で2つ目の会話履歴が構築され、同じ内容を二重回答してしまう。これを防ぐため、
    // 会話履歴の構築・推論・応答ログをセッション単位で直列化する。
    // dispatch registry は既存どおり呼び出し側から受け取る（DiscordGatewayActions と
    // 同じ Arc を共有する必要があるため）。そのためここで使うのは登録簿を持たない
    // `SessionLocks`（#223）。登録簿つきの `SessionRuntime` を持つと、その登録簿を
    // 「共有のもの」と誤認して dispatch 先を差し替えたときに cancel_subtask が
    // 走行中 subtask に届かなくなる。型として存在しなければその取り違えは起きない。
    //
    // #588 Stage 2: ローカル生成をやめ、プロセス全体で 1 つの共有 `SessionLocks`
    // （`AppState::session_locks`）を使う。これで同一セッション（`discord-{agent}-{guild}-{channel}`）
    // の通常メッセージ処理ターンと heartbeat の時間トリガーターンが直列化される。
    let session_locks = state.session_locks();

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
                        // バッファキー = channel だけ（#556）。同一 channel は権限に関わらず 1 本の
                        // バッファに貯める。権限による run の分割は**フラッシュ時**（連続同権限
                        // グループ）に行う。ここでは caller を解決しない（グループ分けは flush 側）。
                        let entry = debounce_buffers
                            .entry(debounce_window_key(&msg))
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
                        caller,
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
                        let event_tx_c = event_tx.clone();
                        let registry_c = subtask_registry.clone();
                        let sess = session_id.clone();
                        session_locks.spawn_serialized(sess, async move {
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
                                event_tx_c,
                                registry_c,
                                caller,
                            )
                            .await;
                        });
                        // 実行ハンドルは返ってこない（応答は待たない = 受信ループを止めない）。
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
                        caller,
                    }) => {
                        // SubtaskCompleted と同じ理由でループ内では await しない。
                        let gateway_c = gateway.clone();
                        let state_c = state.clone();
                        let ga_c = gateway_actions.clone();
                        let sess = session_id.clone();
                        session_locks.spawn_serialized(sess, async move {
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
                                caller,
                            )
                            .await;
                        });
                    }
                    Some(LoopEvent::TimedFire {
                        session_id,
                        agent_id,
                        channel_id,
                        channel_id_str,
                        guild_id,
                        is_dm,
                        prompt,
                        caller,
                    }) => {
                        // SubtaskCompleted と同じ理由でループ内では await しない。同一セッションの
                        // 直列化はセッションロックが担保する（通常メッセージ・継続ターンと同じロック）。
                        let gateway_c = gateway.clone();
                        let state_c = state.clone();
                        let ga_c = gateway_actions.clone();
                        let voice_c = voice.clone();
                        let event_tx_c = event_tx.clone();
                        let registry_c = subtask_registry.clone();
                        let sess = session_id.clone();
                        session_locks.spawn_serialized(sess, async move {
                            process_timed_fire(
                                session_id,
                                agent_id,
                                channel_id,
                                channel_id_str,
                                guild_id,
                                is_dm,
                                prompt,
                                gateway_c,
                                state_c,
                                ga_c,
                                voice_c,
                                event_tx_c,
                                registry_c,
                                caller,
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
                // #665: デバウンス窓が満了し、溜めていた受信の処理へ進む段。ここより前は「受信を溜めて
                // いる」正常状態で、ここが「ターン処理へ入る」入口。session_id はまだ無い（agent 毎に
                // process_incoming_message 内で決まる）ので相関はチャンネルキーで出す。
                debug!(
                    channel = %key,
                    messages = messages.len(),
                    stage = "debounce_flush",
                    "turn: デバウンス満了 → 受信処理開始"
                );
                // #543 / #556: 全メッセージを**個別に**記録する（送信者の帰属を保つ）。run は
                // **到着順のまま「連続した同一 trust_level」で切ったグループ**ごとに 1 回だけ、
                // そのグループ内の**内容のある最後のメッセージ**が起こす。run は DB から会話全体を
                // 読むので、文脈は全員分・正しい帰属で入る。
                //
                // **なぜ「連続同権限グループ」か（#556）**: バッファを channel だけで 1 本にすると
                // owner と外部ユーザーが混ざる。窓を丸ごと 1 run にすると caller が最後の送信者で
                // 決まり owner 指示が降格しうる。権限ごとに並行バッファへ割ると同じ文脈に 2 回 run が
                // 起きて増幅する。連続同権限だけをグループにすれば、**グループ内は権限が揃うので
                // caller が一意**（降格しない）で、かつ**別権限は別グループ＝別 run**（混ざらない）。
                // 例: owner→外部→co_agent は [owner][外部][co_agent] の 3 run、owner→co_agent→外部は
                // [owner,co_agent][外部] の 2 run。
                //
                // **#489 未修正の今の実運用**: co_agent は resolve_caller で `Agent`(=0) に落ちるため、
                // owner→co_agent は同権限にならず別グループになる。ここでのグループ分けは
                // trust_level が揃った場合の挙動で、#489 が直れば owner 等価(=2)として合流する。
                // 誰か（caller / trust_level）と「何本の run にするか」は core。
                // 束を 1 回投げ、分割〜record_only〜ターン対象は accept_inbound が決める（Q13）。
                let channel_ids: Vec<(String, String)> = messages
                    .iter()
                    .map(|m| match &m.source {
                        opencrab_gateway::MessageSource::Discord {
                            guild_id,
                            channel_id,
                        } => (guild_id.clone(), channel_id.clone()),
                        _ => (String::new(), String::new()),
                    })
                    .collect();
                let mut admitted: Vec<Option<AdmittedInbound>> = vec![None; messages.len()];
                let mut run_at = vec![false; messages.len()];
                let mut read_at = vec![false; messages.len()];
                let accept_err = {
                    let works: Vec<InboundWork<'_>> = messages
                        .iter()
                        .enumerate()
                        .map(|(i, m)| {
                            let (guild, ch) = &channel_ids[i];
                            InboundWork {
                                event: NormalizedInboundEvent {
                                    sender_id: &m.sender.id,
                                    channel_id: ch,
                                    guild_id: guild,
                                },
                                has_content: incoming_has_content(m),
                                kind_label: "",
                                author_key: &m.sender.id,
                            }
                        })
                        .collect();
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
                        &works,
                        &owner_discord_id,
                        &agent_ids,
                        &lookups,
                        None,
                        |_| (),
                        |i, adm| admitted[i] = Some(adm.clone()),
                        |i, _, read| {
                            run_at[i] = true;
                            for &r in read {
                                read_at[r] = true;
                            }
                        },
                    )
                };
                if let Err(e) = accept_err {
                    if matches!(
                        e,
                        opencrab_actions::InboundDrop::Message(InboundMessageDrop::DmNotTrusted)
                    ) {
                        let key = format!("dm_gate:{}", messages[0].sender.id);
                        if should_emit_drop_log(
                            &DROP_LOG_LAST,
                            &key,
                            Instant::now(),
                            DROP_LOG_THROTTLE,
                        ) {
                            info!(
                                sender = %messages[0].sender.id,
                                reason = "dm_sender_not_trusted",
                                "受信DMを破棄: 設定によりどのエージェントも送信者を信頼していない"
                            );
                        }
                    }
                    continue;
                }

                if messages.len() > 1 {
                    info!(
                        channel = %key,
                        messages = messages.len(),
                        groups = run_at.iter().filter(|r| **r).count(),
                        "Debounced (channel): recording all, running once per consecutive-privilege group"
                    );
                }

                for (i, msg) in messages.into_iter().enumerate() {
                    let Some(plan) = admitted[i].clone() else {
                        continue;
                    };
                    process_incoming_message(
                        msg,
                        gateway.clone(),
                        state.clone(),
                        agent_ids.clone(),
                        gateway_actions.clone(),
                        owner_discord_id.clone(),
                        session_locks.clone(),
                        skip_agents_with_dedicated_gateway,
                        v3_liveness.clone(),
                        voice.clone(),
                        event_tx.clone(),
                        subtask_registry.clone(),
                        !run_at[i],
                        read_at[i],
                        Some(plan),
                    )
                    .await;
                }
            }
        }
    }

    info!("Discord event loop v3 ended");
}
