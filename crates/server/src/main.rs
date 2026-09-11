use std::sync::Arc;

use opencrab_server::create_router_with_gate;

#[path = "main/background.rs"]
mod background;
#[path = "main/bootstrap.rs"]
mod bootstrap;
mod intake_process;
mod scheduler;

#[cfg(test)]
#[path = "../tests/baseline_support/bin_test_support.rs"]
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bootstrap::BootstrapContext {
        cfg,
        extgate,
        gate_socket,
        heartbeat_config_tx,
        heartbeat_config_rx,
        mut state,
        ..
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

    // 前プロセスから残った保留対話を**期限切れとして明示的に閉じる**（#196）。
    // 保留状態のメモリ上の登録簿はプロセスと寿命を共にするので、ここに残っている
    // `pending` 行は誰も応答を受け取れない。無言で放置すると「ボタンを押しても何も
    // 起きない」行が DB に溜まり続けるため、起動時に 1 度だけ閉じてログに残す。
    // transport に依存しない処理なので、Discord 機能フラグやゲートウェイの稼働有無の
    // **外**で行う（web / REST だけの構成でも効く）。
    {
        use opencrab_actions::AgentRuntime as _;
        state.cleanup_stale_interactions();
    }

    let _watcher_handle = background::spawn_background_tasks(
        &state,
        &cfg,
        &gate_socket,
        heartbeat_config_tx,
        heartbeat_config_rx,
    );

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
            if let Err(e) = opencrab_extgate::serve_uds(listen_state, runtime, path).await {
                tracing::error!(error = %e, "extgate listener halted");
                std::process::exit(1);
            }
        });
    }

    let app = create_router_with_gate(state, extgate);

    let addr = format!("0.0.0.0:{}", cfg.gateway.rest.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Server listening on {}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(wait_for_os_shutdown())
        .await?;

    Ok(())
}

/// SIGINT（Ctrl-C）または SIGTERM を待つ。graceful shutdown のトリガに使う。
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

// #588 TimedFire / #599: ハートビートの発火本体は `opencrab_server::heartbeat_fire::run_one_heartbeat`
// （時刻が来たら発火先ゲートウェイのループへ `TimedFire` を 1 本流すだけの free 関数）に集約した。lib へ
// 置いてあるので scheduler（時刻発火）と `run_my_heartbeat`（手動発火）が同じ 1 つの関数を共有する。
// 専用のターン実装・専用配送（旧 `heartbeat_delivery.rs`）・scheduler 側の継続ターン機構は撤去し、以降の
// ターンはexternal gatewayの通常delivery経路が回す。
// 指示文の整形テストは `heartbeat_fire` の `#[cfg(test)]` にある。
