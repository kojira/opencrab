use crate::{intake_process, scheduler};
use opencrab_server::AppState;

/// Starts the transport-independent maintenance, intake, scheduler, self-check,
/// and configuration-watcher tasks in their required startup order.
#[allow(clippy::let_and_return)] // Keep the original watcher binding and its main-lifetime handoff.
pub(super) fn spawn_background_tasks(
    state: &AppState,
    _gate_socket: &Option<std::path::PathBuf>,
) -> std::thread::JoinHandle<()> {
    // Every loop below reads its settings (enabled / interval / retention ...) from the live
    // config on every tick (D-1113), so they are always started and editing the config file
    // turns them on or off without a restart.

    // メモリインデックスのアイドル時メンテナンス（増分ビルドの取りこぼし回収 /
    // キーワードバックフィル / 月次ロールアップ / sleep）。全エージェントを毎 tick 巡回。
    opencrab_server::memory_maintenance::spawn_memory_maintenance_loop(state.clone());

    // 古い llm_logs の zip アーカイブ（#337）。メンテナンスループが per-agent かつ
    // 高頻度（既定 600 秒）なのに対し、こちらは全 llm_logs を対象にした日次の重い I/O
    // なので別ループにする。
    opencrab_server::llm_log_archive::spawn_llm_log_archive_loop(state.clone());

    // 退避ファイル（workspace/tmp）の掃除（#711）。退避経路は書くだけで消す実装が無く、
    // ファイルが無限に増える。全エージェントの `workspace/tmp/` を日次で巡回し、mtime が
    // 保持日数より古い**通常ファイルのみ**を個別 remove_file で消す（グロブ・再帰なし）。
    opencrab_server::offload_cleanup::spawn_offload_cleanup_loop(state.clone());

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

    let _watcher_handle =
        opencrab_server::hot_reload::start_config_watcher("config", state.clone());

    _watcher_handle
}
