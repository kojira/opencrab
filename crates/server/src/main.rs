use std::sync::Arc;

use opencrab_server::create_router_with_gate;

#[path = "main/background.rs"]
mod background;
#[path = "main/bootstrap.rs"]
mod bootstrap;
mod intake_process;
mod scheduler;

#[cfg(test)]
mod bin_test_support;

// #599: ハートビートの発火本体（`run_one_heartbeat`）と表示ラベル
// `HEARTBEAT_NOSTR_CHANNEL_LABEL` は lib（`opencrab_server::heartbeat_fire`）へ移した。
// scheduler（時刻発火）と `run_my_heartbeat`（手動発火）が同じ 1 つの関数を共有するため。

/// config名またはUUIDのagent_idを、DBのUUIDに解決する。
/// "crab"のような名前が渡された場合、find_agentsで検索してUUIDを返す。
fn resolve_agent_id(conn: &rusqlite::Connection, agent_id: &str) -> String {
    // まず直接lookupを試みる
    if let Ok(Some(_)) = opencrab_db::queries::get_agent(conn, agent_id) {
        return agent_id.to_string();
    }
    // 名前で検索（完全一致またはUUID前方一致のみ。部分一致は複数エージェント時に誤マッチするため使わない）
    if let Ok(agents) = opencrab_db::queries::find_agents(conn, agent_id) {
        if let Some((uuid, _name)) = agents.iter().find(|(id, name)| {
            id.starts_with(agent_id) || name.to_lowercase() == agent_id.to_lowercase()
        }) {
            tracing::info!(config_id = %agent_id, uuid = %uuid, "Resolved agent_id config name to UUID (exact match)");
            return uuid.clone();
        }
    }
    // シングルエージェントフォールバック: DBに登録済みのエージェントが1つだけの場合はそれを使う
    // (config名"crab"などがDBの名前と一致しない場合の対応)
    if let Ok(all_agents) = opencrab_db::queries::find_agents(conn, "") {
        if all_agents.len() == 1 {
            let (uuid, name) = &all_agents[0];
            tracing::info!(
                config_id = %agent_id,
                uuid = %uuid,
                name = %name,
                "Resolved agent_id to only registered agent (single-agent fallback)"
            );
            return uuid.clone();
        }
    }
    tracing::warn!(agent_id = %agent_id, "Could not resolve agent_id to UUID, using as-is");
    agent_id.to_string()
}

/// discord-gateway 子 binary の解決順（DESIGN-DISCORD-GATE §0: 1 process = 1 agent）。
/// 1. 環境変数 `OPENCRAB_DISCORD_GATEWAY_BIN`（明示指定・運用者が場所を固定できる）。
/// 2. server 実行ファイルと同じディレクトリの `discord-gateway`（cargo の同一 target/ 配置）。
/// 3. 上記が無ければ `discord-gateway`（PATH 解決に委ねる）。
#[cfg(feature = "discord")]
fn resolve_discord_gateway_bin() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("OPENCRAB_DISCORD_GATEWAY_BIN") {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("discord-gateway");
            if sibling.exists() {
                return sibling;
            }
        }
    }
    std::path::PathBuf::from("discord-gateway")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bootstrap::BootstrapContext {
        cfg,
        extgate,
        gate_socket,
        #[cfg(feature = "discord")]
        gate_socket_for_discord,
        #[cfg(feature = "discord")]
        attachment_inbox_root,
        #[cfg(feature = "discord")]
        discord_ingress,
        #[cfg_attr(not(feature = "discord"), allow(unused_variables))]
        effective_voice,
        #[cfg(feature = "nostr")]
        nostr_ingress,
        #[cfg(feature = "nostr")]
        nostr_master_key,
        #[cfg(feature = "nostr")]
        start_nostr,
        heartbeat_config_tx,
        heartbeat_config_rx,
        mut state,
    } = bootstrap::initialize()?;

    // #628: transport の発火先 descriptor を**生存非依存で**登録する（ゲートウェイの起動有無・
    // 資格情報の有無に関わらず常時。受理判定・ゲート理由表示・parse はゲートウェイ停止中でも
    // 要る）。sink（生存で register/unregister）とは別の登録で、ここは起動ブロックの**外**に
    // 置く（#627 で「Discord 有効ブロックの中に置く」設計が隔離環境で発火しない罠になった）。
    // 各 descriptor の実装はその transport の crate にあり、登録の源は 1 本化した
    // `register_production_descriptors` だけ（main.rs / test_app_state / scheduler の test_router /
    // 登録簿を反復する generic テストがすべてこれを呼ぶ）。散らすと本番へ足してテスト側への追記を
    // 忘れる隙ができ、prefix 衝突が本番でだけ顕在化しうる（#628 のブロッカー対応）。
    {
        let conn = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock failed at startup budget check: {e}"))?;
        opencrab_server::process::ensure_startup_budget_inputs(&conn, &state.default_model)
            .map_err(|e| anyhow::anyhow!("context budget fail-loud at startup: {e}"))?;
    }

    opencrab_server::register_production_descriptors(&state.timed_fire_router);

    // サブタスク lifecycle 通知の実装を配線する（#175 S4）。`spawn_subtask` は gateway
    // 非依存層にあるため、通知先の解決（DB の webhook 設定 + TOML の既定）だけを持つ
    // この実装を `AppState` へ差し込む。Discord ゲートウェイの稼働有無とは独立に効く
    // （web / REST から起動したサブタスクにも lifecycle 通知が出る）。
    #[cfg(feature = "discord")]
    {
        let default_subtask_webhook = state.default_subtask_webhook.clone();
        *state.subtask_lifecycle_notifier.lock().unwrap() = Some(Arc::new(
            opencrab_discord::DiscordWebhookNotifier::new(
                state.db.clone(),
                default_subtask_webhook,
            ),
        )
            as Arc<dyn opencrab_actions::subtask_notify::SubtaskLifecycleNotifier>);
    }

    // 前プロセスから残った保留対話を**期限切れとして明示的に閉じる**（#196）。
    // 保留状態のメモリ上の登録簿はプロセスと寿命を共にするので、ここに残っている
    // `pending` 行は誰も応答を受け取れない。無言で放置すると「ボタンを押しても何も
    // 起きない」行が DB に溜まり続けるため、起動時に 1 度だけ閉じてログに残す。
    // transport に依存しない処理なので、Discord 機能フラグやゲートウェイの稼働有無の
    // **外**で行う（nostr / web / REST だけの構成でも効く）。
    {
        use opencrab_actions::AgentRuntime as _;
        state.cleanup_stale_interactions();
    }

    // Start Discord gateway if configured and feature is enabled.
    #[cfg(feature = "discord")]
    {
        // Per-agent Discord gateway manager（#40: 共有ループが「専用ゲートウェイが
        // 稼働中か」を参照できるよう、共有ゲートウェイへ渡す AppState clone より
        // **前に**生成して配線する。実際の復元は共有ゲートウェイ起動後に行う）。
        //
        // DESIGN-DISCORD-GATE §8.1: V3 gateway process の liveness probe。core の live registry
        // （`ExtgateState::agent_has_live_gateway`）を正とし、DB enabled ではない。probe/DB ロック
        // 失敗は false（＝退かない）へ倒れる。この 1 本を (1) legacy per-agent ループの二重受信ゲート
        // （manager が per-message で見る）と (2) 共有 message_loop 側の V3AwareGateway（is_running に
        // OR）の**両方**へ渡す。両型は同一の具象型（`Arc<dyn Fn(&str)->bool + Send + Sync>`）。
        let extgate_for_v3 = extgate.clone();
        let v3_liveness: std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync> =
            std::sync::Arc::new(move |agent_id: &str| {
                extgate_for_v3
                    .agent_has_live_gateway(agent_id, opencrab_actions::gateway_kinds::DISCORD)
            });

        // #603: 時刻発火の受け口レジストリは `new` の**必須引数**。忘れるとコンパイルエラーに
        // なる（#602 は Option + builder の呼び忘れで Discord の発火が全 skip し本番が止まった）。
        // v3_liveness も必須引数（DESIGN-DISCORD-GATE §8.1・二重受信防止・配線し忘れ＝本バグ再発）。
        let manager = Arc::new(opencrab_discord::DiscordGatewayManager::new(
            state.clone(),
            state.timed_fire_router.clone(),
            v3_liveness.clone(),
        ));
        // 上位から見える唯一の入口はこの登録簿（#191 段階2 PR3・PR4）。共通操作
        // （起動 / 停止 / 生存確認）も transport 固有の操作（ツール実行の実体 =
        // `gateway_actions_for`）もここから引く。`AppState` の名指しフィールドは無い。
        // 登録簿は `state` の clone 同士で同じ Arc を共有するので、ここで入れた分は
        // 既に clone 済みの state からも見える（#40 の二重処理防止がこれに依存する）。
        //
        // DESIGN-DISCORD-GATE §8.1: 併存期は legacy manager の liveness に **V3 gateway process の
        // liveness を OR** して登録する。共有 message_loop の `served_by_dedicated_gateway`
        // （= 登録簿の is_running）が、legacy でも V3 でもどちらか稼働中なら対象 agent を除外し、
        // 同一 channel での新旧二重応答を防ぐ。V3 liveness は core の live registry を正とする
        // （DB enabled ではない・#40）。probe/DB ロック失敗は false へ倒れ、共有側が処理を続ける。
        // 上で組んだ `v3_liveness` を使い回す（per-agent ゲートと同一 closure）。
        let v3_aware = opencrab_server::dedicated_gateway::V3AwareGateway::new(
            manager.clone(),
            v3_liveness.clone(),
        );
        state.gateways.register(v3_aware);

        let discord_cfg = &cfg.gateway.discord;

        // owner 未設定は「無音で権限モデルが変わる」ので起動時に必ず知らせる。
        // `.env` の OWNER_DISCORD_ID を入れ忘れると `${OWNER_DISCORD_ID}` が空文字に
        // 展開され、設定ファイルを見ても気づけない。
        // 共有ゲートウェイが実際に起動する条件（enabled かつトークンあり）でだけ出す。
        // per-agent ゲートウェイ側の警告は DiscordGatewayManager::start_agent_gateway が出す。
        opencrab_discord::warn_if_shared_gateway_owner_unset(
            discord_cfg.enabled,
            &discord_cfg.token,
            &discord_cfg.owner_discord_id,
        );

        // Fallback: config-based shared gateway (existing behavior).
        // 起動条件は警告条件と同じ述語を使う（条件の二重管理を避ける。理由は
        // `gateway_will_start` の doc コメント参照）。
        if opencrab_discord::gateway_will_start(discord_cfg.enabled, &discord_cfg.token) {
            tracing::info!("Starting Discord gateway (config-based fallback)...");

            // Validate agent IDs against the database
            let valid_agent_ids: Vec<String> = {
                let conn = state.db.lock().unwrap();
                let ids: Vec<String> = discord_cfg
                    .agent_ids
                    .iter()
                    .map(|agent_id| resolve_agent_id(&conn, agent_id))
                    .filter(
                        |agent_id| match opencrab_db::queries::get_agent(&conn, agent_id) {
                            Ok(Some(_)) => true,
                            _ => {
                                tracing::warn!(
                                    "Agent '{}' not found in database, skipping",
                                    agent_id
                                );
                                false
                            }
                        },
                    )
                    .collect();
                // #40: enabled な per-agent Discord 設定を持つエージェントは、専用
                // ゲートウェイの**稼働中**は共有ループが per-message でスキップする
                // （liveness ベース）。ここでリストから除外はしない: 専用側が起動失敗
                // した場合に共有側がフォールバックとして応答を続けるため。
                for agent_id in &ids {
                    if matches!(
                        opencrab_db::queries::get_agent_discord_config(&conn, agent_id),
                        Ok(Some(cfg)) if cfg.enabled
                    ) {
                        tracing::info!(
                            agent_id = %agent_id,
                            "Agent has an enabled per-agent Discord config; shared gateway \
                             will defer to it while its dedicated gateway is running"
                        );
                    }
                }
                ids
            };

            if valid_agent_ids.is_empty() {
                tracing::error!("No valid agents found for Discord gateway, not starting");
            } else {
                let gateway = Arc::new(opencrab_discord::DiscordGateway::new(&discord_cfg.token));
                gateway.start().await?;

                // auto-dispatch の登録簿。停止（`cancel_subtask`）は gateway 非依存層の実装が
                // 同じ Arc を run 経由（`RunRequest::with_dispatch`）で受け取るため、この
                // registry はループへ渡すだけでよい（#157 S2 で gateway_actions からは外した）。
                let subtask_registry: opencrab_actions::SubtaskRegistry =
                    Arc::new(dashmap::DashMap::new());
                let subtask_registry_for_loop = subtask_registry.clone();
                // subtask 完了/進捗の通知はイベントループへの直接送信になった（#39）ため、
                // gateway_actions とループで同じチャンネルを共有する必要がある。
                let (event_tx, event_rx) = opencrab_discord::message_loop::create_event_channel();
                // #588 TimedFire: この共有（TOML）ループを Discord の共有受け口として登録する。
                // per-agent ゲートウェイを持たないエージェントの時刻発火はここへ落ちる（#400 と同型）。
                state.timed_fire_router.register_shared(
                    opencrab_actions::gateway_kinds::DISCORD,
                    Arc::new(opencrab_discord::message_loop::DiscordTimedFireSink {
                        event_tx: event_tx.clone(),
                    }),
                );
                // #601: 登録が起きたことを起動時に 1 行残す（per-agent を持たない体の時刻発火は
                // ここへ落ちる。これが出ない＝共有受け口が無い、を運用で即検知できるように）。
                tracing::info!(
                    transport = "discord",
                    "timed-fire: 受け口を登録（共有 TOML Discord loop）"
                );
                // 設定ファイル由来の通知先フォールバック（#157 S5 で `AppState` へ
                // 持ち上げ済み）。Discord にはもう `ensure_*` しか残っていないが、
                // 解決経路が全 transport で同じ値を見ることをここで担保する。
                let default_subtask_webhook = state.default_subtask_webhook.clone();
                // VC 対話（STT/TTS）: 実効設定（DB オーバーライド適用済み）で構築する。
                // プロバイダ構築失敗（未知の provider 等）は起動を止めず警告して無効化。
                let voice_cfg = &effective_voice;
                let voice_manager: Option<
                    Arc<opencrab_discord::voice_session::VoiceSessionManager>,
                > = if voice_cfg.enabled {
                    match (
                        opencrab_voice::build_stt(&voice_cfg.stt),
                        opencrab_voice::build_tts(&voice_cfg.tts),
                    ) {
                        (Ok(stt), Ok(tts)) => {
                            tracing::info!(
                                stt = %voice_cfg.stt.provider,
                                tts = %voice_cfg.tts.provider,
                                "voice (VC) conversation enabled"
                            );
                            let mgr = opencrab_discord::voice_session::VoiceSessionManager::new(
                                gateway.voice(),
                                stt,
                                tts,
                                voice_cfg.tts.clone(),
                                voice_cfg.stt.language.clone(),
                                event_tx.clone(),
                                gateway.http().clone(),
                            );
                            // ダッシュボードからの設定変更をホットスワップで受ける
                            *state.voice_runtime.lock().unwrap() =
                                Some(mgr.clone() as Arc<dyn opencrab_voice::VoiceRuntime>);
                            Some(mgr)
                        }
                        (stt, tts) => {
                            if let Err(e) = stt {
                                tracing::warn!("voice STT provider init failed: {e}");
                            }
                            if let Err(e) = tts {
                                tracing::warn!("voice TTS provider init failed: {e}");
                            }
                            None
                        }
                    }
                } else {
                    None
                };

                let gateway_actions_base = opencrab_discord::DiscordGatewayActions::new(
                    gateway.http().clone(),
                    state.db.clone(),
                    state.workspace_base.clone(),
                    default_subtask_webhook,
                )
                .with_event_tx(event_tx.clone())
                .with_owner_discord_id(discord_cfg.owner_discord_id.clone());
                let gateway_actions: Arc<dyn opencrab_gateway::GatewayActions> =
                    Arc::new(match &voice_manager {
                        Some(v) => gateway_actions_base.with_voice(v.clone()),
                        None => gateway_actions_base,
                    });

                let discord_state = state.clone();
                let owner_discord_id = discord_cfg.owner_discord_id.clone();
                tokio::spawn(async move {
                    opencrab_discord::run_discord_loop(
                        gateway,
                        discord_state,
                        valid_agent_ids,
                        gateway_actions,
                        owner_discord_id,
                        None, // pending_registry
                        Some((event_tx, event_rx)),
                        // 共有（TOML）ゲートウェイ: ランタイムに per-agent 設定が
                        // enable されたエージェントはメッセージ処理時にスキップ（#40）。
                        true,
                        // V3 二重受信ゲートは per-agent ループ専用。共有側は上の
                        // `served_by_dedicated_gateway`（V3AwareGateway が V3 liveness を OR）で
                        // 既に V3 稼働 agent を除外済みなので None（二重ゲート回避・DESIGN-DISCORD-GATE §8.1）。
                        None,
                        voice_manager,
                        subtask_registry_for_loop,
                    )
                    .await;
                });

                tracing::info!(
                    agents = ?discord_cfg.agent_ids,
                    owner = %discord_cfg.owner_discord_id,
                    "Discord gateway started (config-based)"
                );
            }
        }

        // **1 つ目の復元位置。** ここまでに登録簿へ入っていて、まだ復元していない
        // ゲートウェイを登録順に復元する（#191 段階2 PR5）。この時点で登録済みなのは
        // 上で登録した 1 つだけなので、実際に走る内容は移設前の
        // `manager.restore_from_db()` と 1 対 1。
        //
        // **この位置は動かせない**（走査を最後の 1 回に畳めない理由でもある）:
        // 1. 復元は共有（TOML）ゲートウェイの**起動後**。起動直後の短い窓では共有側が
        //    メッセージを処理し、専用ゲートウェイが上がり次第 per-message スキップが効く。
        // 2. 下の起動時診断（どの体で Discord ハンドルが解決できるか）が、この復元の
        //    **完了**を前提にしている。#400 以降、実際の解決は配送のたびに行うので
        //    「復元が後ろへずれると発話が共有ゲートウェイの HTTP のまま固定される」
        //    という取り返しのつかない依存は無くなったが、診断の意味は復元後にしかない。
        state.gateways.restore_pending().await;

        tracing::info!("Per-agent Discord gateway manager initialized");
    }

    let _watcher_handle = background::spawn_background_tasks(
        &state,
        &cfg,
        &gate_socket,
        heartbeat_config_tx,
        heartbeat_config_rx,
    );

    // Per-agent Nostr sub-gateway マネージャ（discord と同様に、state clone より前に
    // 生成して配線する）。
    //
    // #620: **マスターキーが在るときだけ**登録する。無ければ Nostr は起動しない（送信も受信も
    // 止まる）。Nostr 未設定の構成ではそもそもマスターキー不要なので、ここを飛ばして通常起動する。
    // PR-1B: Nostr は会話ゲートなので nostr feature の内側。外した構成ではこのブロック自体が無い。
    #[cfg(feature = "nostr")]
    if let Some(master_key) = nostr_master_key.clone() {
        // nostaro は**エージェントの workspace ルートを cwd にして**起動する（#299）。
        // `execute_shell` / `ws_*` と同じ `agent.workspace_path` を渡して基準を揃える
        // （`nostr_run event --file <相対>` / `--out <相対>` がそれらと噛み合う）。
        //
        // #620: 本鍵は config へ書かず、`base_command` が spawn ごとに **本鍵プロバイダ**で DB の
        // 暗号文を復号して env 注入する。生成鍵ファイルの復号用に **マスターキー**も注入する。
        let provider = opencrab_nostr::db_main_key_provider(state.db.clone(), master_key.clone());
        let cli = opencrab_nostr::NostaroCli::new()
            .with_workspace_base(state.workspace_base.clone())
            .with_master_key(master_key)
            .with_main_key_provider(provider);
        // #588 TimedFire / #603: 時刻発火の受け口レジストリは `new` の必須引数（Discord と同型・
        // per-agent→共有の解決はルータが行う）。忘れるとコンパイルエラーになる。
        let db_for_provision = state.db.clone();
        let db_for_instance = state.db.clone();
        let db_for_revise = state.db.clone();
        let manager_builder = opencrab_nostr::NostrGatewayManager::new(
            state.clone(),
            state.timed_fire_router.clone(),
        )
        .with_cli(cli)
        .with_ingress(nostr_ingress)
        .with_provisioner(Arc::new(move |agent_id, self_pk, config, watches| {
            let mut conn = db_for_provision
                .lock()
                .map_err(|_| anyhow::anyhow!("db lock for nostr provision"))?;
            opencrab_server::nostr_provision::provision_nostr_gate(
                &mut conn,
                agent_id,
                self_pk,
                config,
                watches,
                opencrab_extgate::now_nanos(),
            )?;
            Ok(())
        }))
        .with_instance_provisioner(Arc::new(move |agent_id, self_pk, config, watches| {
            let mut conn = db_for_instance
                .lock()
                .map_err(|_| anyhow::anyhow!("db lock for nostr instance"))?;
            opencrab_server::nostr_provision::provision_nostr_instance(
                &mut conn,
                agent_id,
                self_pk,
                config,
                watches,
                opencrab_extgate::now_nanos(),
            )
        }))
        .with_reviser(Arc::new(move |agent_id, self_pk, config, watches| {
            let mut conn = db_for_revise
                .lock()
                .map_err(|_| anyhow::anyhow!("db lock for nostr revise"))?;
            opencrab_server::nostr_provision::revise_nostr_gate(
                &mut conn,
                agent_id,
                self_pk,
                config,
                watches,
                opencrab_extgate::now_nanos(),
            )
        }));
        let manager: opencrab_server::SharedNostrManager = Arc::new(manager_builder);
        let store = manager.allow_store().clone();
        let store_sets = store.clone();
        extgate.set_nostr_said_admit(Arc::new(move |agent_id, author_id, text| {
            use opencrab_extgate::{ErrorCode, GateError, NostrSaidDecision};
            use opencrab_nostr::{admit_nostr_said, AdmitSaidError, IngressRoute};
            let Some(allow) = store.get_allow(agent_id) else {
                return Err(GateError::store());
            };
            let Some(self_pk) = store.self_pubkey(agent_id) else {
                return Err(GateError::store());
            };
            match admit_nostr_said(text, author_id, &self_pk, &allow) {
                // 診断のため detail を載せる（本文/鍵/path は入れない・カテゴリのみ）。anchor の
                // key 集合/型不整合（gateway↔core のバージョン齟齬など）を素早く切り分けられる。
                Err(AdmitSaidError::BadAnchor) => Err(GateError::with_detail(
                    ErrorCode::BadRequest,
                    "nostr V1 anchor parse failed (unknown/missing key or bad type)",
                )),
                Err(AdmitSaidError::Drop { anchor, .. }) => Ok(NostrSaidDecision::Drop {
                    bundle: nostr_bundle_from_anchor(&anchor)?,
                }),
                Ok(anchor) => Ok(NostrSaidDecision::Accept {
                    watch_id: anchor.watch_id,
                    immediate: anchor.route == IngressRoute::Immediate,
                    bundle: nostr_bundle_from_anchor(&anchor)?,
                }),
            }
        }));
        extgate.set_nostr_watch_sets(Arc::new(move |agent_id| {
            store_sets
                .get_allow(agent_id)
                .map(|allow| opencrab_extgate::NostrWatchSets {
                    followees: allow.followees,
                    owner: allow.owner,
                    co_agents: allow.co_agents,
                    trusted_users: allow.trusted_users,
                })
        }));
        let workspace_base = state.workspace_base.clone();
        extgate.set_nostr_workspace(Arc::new(move |agent_id| {
            opencrab_core::workspace::resolve_agent_workspace(&workspace_base, agent_id).ok()
        }));
        let relay_runner = state.clone();
        extgate.set_nostr_relay(Arc::new(move |agent_id, text| {
            use opencrab_nostr::NostrAgentRunner;
            if let Some(target) = relay_runner.resolve_nostr_relay_target(agent_id) {
                relay_runner.relay_inbound_notification(&target, text);
            }
        }));
        // 共通操作も transport 固有の操作（nostaro の鍵生成 = `key_provisioning`）も
        // この登録簿から引く（#191 段階2 PR3・PR4）。名指しフィールドは無い。
        state.gateways.register(manager);
    } else {
        tracing::info!(
            start_nostr,
            "Nostr サブシステムは起動しない（マスターキー未設定 / 不正）。Nostr 未設定の構成なら正常。"
        );
    }

    // **2 つ目の復元位置**（ルータ構築の直前 / #191 段階2 PR5）。ここまでで未復元なのは
    // 直前に登録した Nostr だけなので（Discord は上のブロックで復元済み・MCP は登録簿に
    // 入れない）、移設前の `manager.restore_from_db()` と 1 対 1。
    //
    // Discord を落とした構成（`--no-default-features`）では 1 つ目の走査ごと消えるため、
    // ここが唯一の復元位置になる。**新しい transport を足すときも呼び出し口は増えない**:
    // 復元させたい位置より前で `register` すればよい。
    state.gateways.restore_pending().await;

    // Per-agent MCP 接続マネージャ。enabled なサーバへ起動時に接続する。
    //
    // **transport 登録簿（`state.gateways`）には入れない**（#191 段階2）。MCP は受信を
    // 持たず、エージェントへ道具を供給する側で transport ではない。道具の注入は
    // 深さ 0（親ターン）限定という遮断が効いており、「受信を持つ transport」と同じ
    // 登録簿に混ぜるとその前提が崩れる。名指しフィールドのまま残す。
    {
        let manager: opencrab_server::SharedMcpManager =
            Arc::new(opencrab_mcp::McpClientManager::new(state.db.clone()));
        state.mcp_manager = Some(manager.clone());
        manager.restore_from_db().await;
        // 自己修復: 切断された（クラッシュ/終了した）サーバを周期的に再接続する。
        let sweeper = manager.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.tick().await; // 最初の即時発火を捨てる
            loop {
                tick.tick().await;
                sweeper.reconnect_dead().await;
            }
        });
    }

    if let Some(path) = gate_socket {
        let listen_state = extgate.clone();
        let runtime = state.clone();
        // #925: V3 heartbeat 受け口（extgate）を共有 sink として登録。`extgate` と `runtime`（AppState）
        // が揃う唯一の点。発火先の session→binding 解決と live 判定は sink 内で行う（§1.5・fail-loud）。
        // 前例: 共有 Discord loop の register_shared（上方）。
        state.timed_fire_router.register_shared(
            opencrab_extgate::EXTGATE_TIMED_FIRE_KIND,
            std::sync::Arc::new(opencrab_extgate::ExtgateTimedFireSink::new(
                extgate.clone(),
                state.clone(),
            )),
        );
        tracing::info!(
            transport = opencrab_extgate::EXTGATE_TIMED_FIRE_KIND,
            "timed-fire: 受け口を登録（V3 extgate・heartbeat）"
        );
        tokio::spawn(async move {
            if let Err(e) = opencrab_extgate::serve_uds(
                listen_state,
                runtime,
                opencrab_server::caller_identity::resolve_caller_identity_with_owner,
                path,
            )
            .await
            {
                tracing::error!(error = %e, "extgate listener halted");
                std::process::exit(1);
            }
        });
    }

    // Discord V3 点火（DESIGN-DISCORD-GATE §8.1）。legacy は何もしない（既存挙動＝spawn なし）。
    // v3_shadow/v3 のとき、enabled な agent_discord_config 各体について instance（v3 は binding も）を
    // 敷き、discord-gateway プロセスを起こす。**UDS listener を spawn した後**に行う（子が core socket へ
    // 接続できるように。子側 InstanceClient は接続を再試行する）。
    // #865: discord-gateway 子プロセスの監視/再起動/後始末（[`discord_supervisor`]）を配線する。
    // shutdown 信号（SIGINT/SIGTERM）でこの `watch` を立てると、各 supervisor は再起動せず子を
    // terminate する（孤児防止）。legacy（provisions_instance=false）のときは None のまま。
    #[cfg(feature = "discord")]
    let mut discord_shutdown_tx: Option<tokio::sync::watch::Sender<bool>> = None;
    #[cfg(feature = "discord")]
    if discord_ingress.provisions_instance() {
        use anyhow::Context as _;
        use opencrab_server::discord_provision::{ignite_discord_instances, DiscordPlacementPlan};
        use opencrab_server::discord_supervisor::{
            supervise, GatewayChildSpawner, SupervisorConfig,
        };

        // placement.json（非秘密）の出力先。DB と同じボリューム（内蔵ディスクに置かない方針）。
        let placement_dir = std::path::Path::new(&cfg.database.path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("gate")
            .join("discord");
        // 子 binary の解決: env 上書き → server 実行ファイルの隣 → PATH の "discord-gateway"。
        let bin = resolve_discord_gateway_bin();
        let core_socket = gate_socket_for_discord.clone();

        // supervisor 群の shutdown 信号。SIGINT/SIGTERM で `true` を送ると各 supervisor は再起動せず
        // 子を terminate する。
        let (sd_tx, sd_rx) = tokio::sync::watch::channel(false);
        discord_shutdown_tx = Some(sd_tx);
        let supervisor_cfg = SupervisorConfig::default();

        let spawn = move |plan: &DiscordPlacementPlan, bot_token: &str| -> anyhow::Result<()> {
            let Some(core_socket) = core_socket.as_deref() else {
                anyhow::bail!(
                    "gate.listen_socket 未設定のため discord-gateway を起動できない（V3 は UDS 必須）"
                );
            };
            // placement.json（秘密なし。bot token は載せない）。再起動でも同じ file を再 exec するので
            // ここで 1 度だけ書けばよい。
            let placement = serde_json::json!({
                "core_socket": core_socket,
                "attachment_spool_root": attachment_inbox_root,
                "instances": [{
                    "instance_id": plan.instance_id,
                    "revision": plan.revision,
                    "addresses": plan.addresses,
                    "config_b64": plan.config_b64,
                }],
            });
            std::fs::create_dir_all(&placement_dir)
                .with_context(|| format!("placement dir 作成失敗: {}", placement_dir.display()))?;
            let path = placement_dir.join(format!("{}.json", plan.agent_id));
            std::fs::write(&path, serde_json::to_vec_pretty(&placement)?)
                .with_context(|| format!("placement 書き出し失敗: {}", path.display()))?;

            // detach をやめ、監視付き supervisor を起こす。bot token は **子の env のみ**（親 env も
            // argv も汚さない・ログにも出さない）で GatewayChildSpawner が注入する。supervisor は
            // 子の終了検知・指数バックオフ再起動・shutdown 時の terminate を担う（#865）。子が死んで
            // 再接続し直すと #866 の liveness probe が再び true になり legacy が退く（外形不減を維持）。
            let spawner = std::sync::Arc::new(GatewayChildSpawner::new(
                bin.clone(),
                path,
                bot_token.to_string(),
                plan.agent_id.clone(),
            ));
            tokio::spawn(supervise(spawner, supervisor_cfg.clone(), sd_rx.clone()));
            tracing::info!(
                agent_id = %plan.agent_id,
                bin = %bin.display(),
                "discord-gateway supervisor 起動（監視/再起動/後始末つき・token は env 注入）"
            );
            Ok(())
        };

        match state.db.lock() {
            Ok(mut conn) => {
                match ignite_discord_instances(
                    &mut conn,
                    discord_ingress,
                    opencrab_extgate::now_nanos(),
                    &spawn,
                ) {
                    Ok(report) => tracing::info!(
                        ingress = discord_ingress.as_str(),
                        provisioned = report.provisioned.len(),
                        spawned = report.spawned.len(),
                        skipped = report.skipped.len(),
                        "discord V3 点火完了"
                    ),
                    Err(e) => {
                        tracing::error!(error = %e, "discord V3 点火に失敗（起動は継続）")
                    }
                }
            }
            Err(_) => tracing::error!("discord V3 点火: db lock 取得失敗"),
        }
    }

    let app = create_router_with_gate(state, extgate);

    let addr = format!("0.0.0.0:{}", cfg.gateway.rest.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Server listening on {}", addr);

    // #865: SIGINT/SIGTERM を受けたら axum を drain しつつ discord-gateway 子を terminate する
    // （孤児プロセス防止）。signal が来なければ従来どおり serve は戻らない（挙動不変）。
    #[cfg(feature = "discord")]
    {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                wait_for_os_shutdown().await;
                tracing::info!(
                    "shutdown signal received: draining and terminating discord-gateway children"
                );
                if let Some(tx) = &discord_shutdown_tx {
                    // 各 supervisor は再起動せず子を terminate する。
                    let _ = tx.send(true);
                    // supervisor が子を kill し切る猶予（kill_on_drop も backstop として併用）。
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
            })
            .await?;
    }
    #[cfg(not(feature = "discord"))]
    axum::serve(listener, app).await?;

    Ok(())
}

/// SIGINT（Ctrl-C）または SIGTERM を待つ。graceful shutdown のトリガに使う。
#[cfg(feature = "discord")]
async fn wait_for_os_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                // SIGTERM ハンドラを張れない場合でも Ctrl-C は拾えるようにする。
                tracing::warn!(error = %e, "SIGTERM ハンドラを設置できない（Ctrl-C のみ待つ）");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(feature = "nostr")]
fn nostr_bundle_from_anchor(
    anchor: &opencrab_nostr::V1Anchor,
) -> Result<Option<opencrab_extgate::NostrBundleAdmit>, opencrab_extgate::GateError> {
    use opencrab_extgate::{ErrorCode, GateError, NostrBundleAdmit};
    use opencrab_nostr::IngressRoute;
    if anchor.route != IngressRoute::Bundle {
        return Ok(None);
    }
    match (
        anchor.bundle_id.clone(),
        anchor.index,
        anchor.count,
        anchor.origins.clone(),
    ) {
        (Some(bundle_id), Some(index), Some(count), Some(origins)) => Ok(Some(NostrBundleAdmit {
            bundle_id,
            index,
            count,
            origins,
        })),
        _ => Err(GateError::new(ErrorCode::BadRequest)),
    }
}

// #588 TimedFire / #599: ハートビートの発火本体は `opencrab_server::heartbeat_fire::run_one_heartbeat`
// （時刻が来たら発火先ゲートウェイのループへ `TimedFire` を 1 本流すだけの free 関数）に集約した。lib へ
// 置いてあるので scheduler（時刻発火）と `run_my_heartbeat`（手動発火）が同じ 1 つの関数を共有する。
// 専用のターン実装・専用配送（旧 `heartbeat_delivery.rs`）・scheduler 側の継続ターン機構は撤去し、以降の
// ターンはゲートウェイ既存の通常ルート（Discord=`SubtaskCompleted` / Nostr=`NostrResponder`）が回す。
// 指示文の整形テストは `heartbeat_fire` の `#[cfg(test)]` にある。
