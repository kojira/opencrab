use std::sync::Arc;

use opencrab_server::create_router_with_gate;

#[path = "main/background.rs"]
mod background;
#[path = "main/bootstrap.rs"]
mod bootstrap;
#[cfg(feature = "discord")]
#[path = "main/discord_ignition.rs"]
mod discord_ignition;
mod intake_process;
#[cfg(feature = "nostr")]
#[path = "main/nostr_ignition.rs"]
mod nostr_ignition;
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
    resolve_gateway_bin("OPENCRAB_DISCORD_GATEWAY_BIN", "discord-gateway")
}

fn resolve_gateway_bin(env_name: &str, binary_name: &str) -> std::path::PathBuf {
    if let Ok(p) = std::env::var(env_name) {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(binary_name);
            if sibling.exists() {
                return sibling;
            }
        }
    }
    std::path::PathBuf::from(binary_name)
}

#[cfg(feature = "nostr")]
fn resolve_nostaro_bin() -> std::path::PathBuf {
    std::env::var("OPENCRAB_NOSTARO_BIN")
        .ok()
        .filter(|path| !path.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("nostaro"))
}

#[cfg(any(feature = "discord", feature = "nostr"))]
fn require_resolvable_binary(label: &str, path: &std::path::Path) -> anyhow::Result<()> {
    let found = if path.components().count() > 1 || path.is_absolute() {
        path.is_file()
    } else {
        std::env::var_os("PATH")
            .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(path).is_file()))
    };
    if !found {
        anyhow::bail!("{label} binary is not resolvable: {}", path.display());
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bootstrap::BootstrapContext {
        cfg,
        extgate,
        gate_socket,
        #[cfg(feature = "discord")]
        gate_socket_for_discord,
        #[cfg(feature = "nostr")]
        gate_socket_for_nostr,
        #[cfg(feature = "discord")]
        attachment_inbox_root,
        #[cfg(feature = "discord")]
        #[cfg_attr(not(feature = "discord"), allow(unused_variables))]
        #[cfg(feature = "nostr")]
        nostr_master_key,
        #[cfg(feature = "nostr")]
        start_nostr,
        heartbeat_config_tx,
        heartbeat_config_rx,
        mut state,
    } = bootstrap::initialize()?;

    #[cfg(feature = "discord")]
    let discord_gateway_bin = resolve_discord_gateway_bin();
    #[cfg(feature = "discord")]
    {
        require_resolvable_binary("discord-gateway", &discord_gateway_bin)?;
        if gate_socket_for_discord.is_none() {
            anyhow::bail!("Discord V3 requires an absolute gate.listen_socket");
        }
    }
    #[cfg(feature = "nostr")]
    let nostr_gateway_bin = resolve_gateway_bin("OPENCRAB_NOSTR_GATEWAY_BIN", "nostr-gateway");
    #[cfg(feature = "nostr")]
    let nostaro_bin = resolve_nostaro_bin();
    #[cfg(feature = "nostr")]
    if start_nostr {
        require_resolvable_binary("nostr-gateway", &nostr_gateway_bin)?;
        require_resolvable_binary("nostaro", &nostaro_bin)?;
    }

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

    #[cfg(feature = "discord")]
    let discord_process_controller = discord_ignition::DiscordV3Controller::new(
        &state.db,
        extgate.clone(),
        &cfg.database.path,
        gate_socket_for_discord
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Discord V3 requires gate.listen_socket"))?,
        &attachment_inbox_root,
        &discord_gateway_bin,
    )?;
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
    let mut nostr_process_controller: Option<Arc<nostr_ignition::NostrV3Controller>> = None;
    #[cfg(feature = "nostr")]
    if let Some(master_key) = nostr_master_key.clone() {
        // nostaro は**エージェントの workspace ルートを cwd にして**起動する（#299）。
        // `execute_shell` / `ws_*` と同じ `agent.workspace_path` を渡して基準を揃える
        // （`nostr_run event --file <相対>` / `--out <相対>` がそれらと噛み合う）。
        //
        // #620: 本鍵は config へ書かず、`base_command` が spawn ごとに **本鍵プロバイダ**で DB の
        // 暗号文を復号して env 注入する。生成鍵ファイルの復号用に **マスターキー**も注入する。
        let provider = opencrab_nostr::db_main_key_provider(state.db.clone(), master_key.clone());
        let process_controller = nostr_ignition::NostrV3Controller::new(
            &state.db,
            &cfg.database.path,
            gate_socket_for_nostr
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Nostr V3 requires gate.listen_socket"))?,
            &provider,
            &nostr_gateway_bin,
            &nostaro_bin,
        )?;
        nostr_process_controller = Some(process_controller.clone());
        let cli = opencrab_nostr::NostaroCli::new()
            .with_binary_path(nostaro_bin.to_string_lossy().into_owned())
            .with_workspace_base(state.workspace_base.clone())
            .with_master_key(master_key)
            .with_main_key_provider(provider);
        // #588 TimedFire / #603: 時刻発火の受け口レジストリは `new` の必須引数（Discord と同型・
        // per-agent→共有の解決はルータが行う）。忘れるとコンパイルエラーになる。
        let db_for_provision = state.db.clone();
        let db_for_revise = state.db.clone();
        let manager_builder = opencrab_nostr::NostrGatewayManager::new(
            state.clone(),
            state.timed_fire_router.clone(),
        )
        .with_cli(cli)
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
        // Liveness は manager の keep-alive task ではなく、外部 gateway が extgate へ登録済みかを
        // 正とする。子が crash-loop 中なら false のままで、稼働中と誤報しない。
        let extgate_for_nostr_live = extgate.clone();
        let nostr_live: opencrab_server::dedicated_gateway::V3LivenessProbe =
            Arc::new(move |agent_id| {
                extgate_for_nostr_live
                    .agent_has_live_gateway(agent_id, opencrab_actions::gateway_kinds::NOSTR)
            });
        let v3_only = opencrab_server::dedicated_gateway::V3OnlyGateway::new(manager, nostr_live)
            .with_process(process_controller);
        state.gateways.register(v3_only);
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
    #[cfg(feature = "discord")]
    state.gateways.register(discord_process_controller.clone());

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

    #[cfg(feature = "nostr")]
    if start_nostr {
        nostr_process_controller
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Nostr V3 process controller is unavailable"))?
            .start_all()
            .await?;
    }

    #[cfg(feature = "discord")]
    discord_process_controller.start_all().await?;

    let app = create_router_with_gate(state, extgate);

    let addr = format!("0.0.0.0:{}", cfg.gateway.rest.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Server listening on {}", addr);

    // SIGINT/SIGTERM で HTTP を drain し、監視中の外部 gateway 子を terminate する。
    #[cfg(any(feature = "discord", feature = "nostr"))]
    {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                wait_for_os_shutdown().await;
                tracing::info!("shutdown signal received: terminating gateway children");
                #[cfg(feature = "discord")]
                opencrab_actions::AgentGatewayLifecycle::shutdown_all(
                    discord_process_controller.as_ref(),
                )
                .await;
                #[cfg(feature = "nostr")]
                if let Some(controller) = &nostr_process_controller {
                    opencrab_server::dedicated_gateway::V3ProcessControl::shutdown_all(
                        controller.as_ref(),
                    )
                    .await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            })
            .await?;
    }
    #[cfg(not(any(feature = "discord", feature = "nostr")))]
    axum::serve(listener, app).await?;

    Ok(())
}

/// SIGINT（Ctrl-C）または SIGTERM を待つ。graceful shutdown のトリガに使う。
#[cfg(any(feature = "discord", feature = "nostr"))]
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
