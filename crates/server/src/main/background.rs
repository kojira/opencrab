use crate::{intake_process, scheduler};
use opencrab_server::{config::AppConfig, AppState};

/// Starts the transport-independent maintenance, intake, scheduler, self-check,
/// and configuration-watcher tasks in their required startup order.
#[allow(clippy::let_and_return)] // Keep the original watcher binding and its main-lifetime handoff.
pub(super) fn spawn_background_tasks(
    state: &AppState,
    cfg: &AppConfig,
    _gate_socket: &Option<std::path::PathBuf>,
) -> std::thread::JoinHandle<()> {
    // メモリインデックスのアイドル時メンテナンス（増分ビルドの取りこぼし回収 /
    // キーワードバックフィル / 月次ロールアップ）。全エージェントを毎 tick 巡回。
    if cfg.agent.memory_maintenance_enabled {
        opencrab_server::memory_maintenance::spawn_memory_maintenance_loop(
            state.clone(),
            cfg.agent.memory_maintenance_interval_secs,
        );
    }

    // 古い llm_logs の zip アーカイブ（#337）。メンテナンスループが per-agent かつ
    // 高頻度（既定 600 秒）なのに対し、こちらは全 llm_logs を対象にした日次の重い I/O
    // なので別ループにする。出力先は未指定なら DB ファイルの親 + `archive`（DB と同じ
    // ボリューム = 内蔵ディスクに置かない方針）へ導出する。
    if cfg.llm_log_archive.enabled {
        let archive_dir = if cfg.llm_log_archive.dir.trim().is_empty() {
            std::path::Path::new(&cfg.database.path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("archive")
        } else {
            std::path::PathBuf::from(&cfg.llm_log_archive.dir)
        };
        opencrab_server::llm_log_archive::spawn_llm_log_archive_loop(
            state.db.clone(),
            archive_dir,
            cfg.llm_log_archive.retention_days,
            cfg.llm_log_archive.interval_secs,
        );
    }

    // 退避ファイル（workspace/tmp）の掃除（#711）。退避経路は書くだけで消す実装が無く、
    // ファイルが無限に増える。全エージェントの `workspace/tmp/` を日次で巡回し、mtime が
    // 保持日数より古い**通常ファイルのみ**を個別 remove_file で消す（グロブ・再帰なし）。
    // 発火判定用マーカーは DB ファイルの親（DB と同じボリューム = 内蔵ディスクに置かない）
    // 直下に置き、どのエージェントの tmp とも混ざらないようにする。
    if cfg.offload_cleanup.enabled {
        let marker_dir = std::path::Path::new(&cfg.database.path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        opencrab_server::offload_cleanup::spawn_offload_cleanup_loop(
            state.db.clone(),
            cfg.agent.workspace_path.clone(),
            marker_dir,
            cfg.offload_cleanup.retention_days,
            cfg.offload_cleanup.interval_secs,
        );
    }

    // 外部イベント受信（webhook intake / #454）。
    //
    // 消化ループは heartbeat の起動条件（グローバル有効 or opt-in）に**依存させない**。
    // heartbeat ループは有効なエージェントが居ないと張られず、そこに inbox 消化を相乗り
    // させると webhook 対象エージェントの heartbeat が無効なとき黙って消化されない
    // （silent no-op）。専用ループにして常時起動し、未処理が空なら LLM を呼ばない
    // （per-agent の非空ゲート / 受け入れ基準）。消化ターンは heartbeat の SPEAK 配送を
    // 通さない（webhook 起点の外部 broadcast を避ける）— 詳細は intake_process モジュール doc。
    intake_process::spawn_intake_process_loop(state.clone());
    // catch-up ポーリング（起動時 + 定期）。source アダプタ未設定なら中で即 return する。
    opencrab_server::intake::spawn_intake_catchup_loop(state.clone());

    // 中央スケジューラ（#439 / #437 / #438 / #612・設計 §3）。
    //
    // **単一タスク**が `agent_schedules` を毎ウェイクで読み直し、永続アンカーから正確な次回発火まで
    // 眠り、`scheduler_wake` で即時反映する。時刻が来たら行の `message` を TimedFire イベントとして
    // 発火先ゲートウェイのループへ 1 本流すだけ（`run_one_heartbeat`）で、以降のターン（配送・ロック・
    // 記録・継続）はそのループの**通常ルート**が回す。受け口の解決は `AppState::timed_fire_router`。
    //
    // per-session 直列化ロック（`SessionLocks`）の唯一のインスタンスは `AppState` が持ち
    // （#588 Stage 2・`AppState::session_locks`）、各ゲートウェイの受信ループと共有する。これで
    // 時間トリガーと通常メッセージ処理のターンが同一 session id 上で直列化される。
    {
        let scheduler_state = state.clone();
        tokio::spawn(async move {
            scheduler::run_scheduler(scheduler_state).await;
        });
    }

    let _watcher_handle = opencrab_server::hot_reload::start_config_watcher(
        "config",
        state.db.clone(),
        // #412: 「default_model が変わったか」の基準。稼働中の実効 spec そのものを渡す
        // （上の `format!("{provider}:{model}")` と同じ形でないと永久に不一致になる）。
        state.default_model.clone(),
        state.tools_config.clone(),
    );

    _watcher_handle
}
