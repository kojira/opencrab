use crate::{intake_process, scheduler};
use opencrab_core::heartbeat::HeartbeatConfig;
use opencrab_server::{config::AppConfig, AppState};
use tokio::sync::watch;

/// Starts the transport-independent maintenance, intake, scheduler, self-check,
/// and configuration-watcher tasks in their required startup order.
#[allow(clippy::let_and_return)] // Keep the original watcher binding and its main-lifetime handoff.
pub(super) fn spawn_background_tasks(
    state: &AppState,
    cfg: &AppConfig,
    gate_socket: &Option<std::path::PathBuf>,
    heartbeat_config_tx: watch::Sender<HeartbeatConfig>,
    heartbeat_config_rx: watch::Receiver<HeartbeatConfig>,
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

    // ハートビートの初期設定と live G の watch チャネルは AppState 構築前に作成済み
    // （`heartbeat_config_tx` / `heartbeat_config_rx`）。tx は下の config watcher へ、
    // rx は scheduler へ渡す（AppState には clone 済み）。

    // 中央ハートビートスケジューラ（#439 / #437 / #438 / 設計 §3）へ切替。
    //
    // 旧実装はエージェントごとに `core::heartbeat::heartbeat_loop` を立て、固定グリッド
    // sleep + メモリ位相（`Instant`）で回していた（再起動で位相消失=#439-1・設定変更が
    // 張り直しまで効かない=#437・sleep グリッドと設定間隔の乖離=#438）。ここでは**単一
    // タスク**が `session_heartbeat_config` を毎ウェイクで読み直し、永続アンカーから正確な
    // 次回発火まで眠り、`scheduler_wake` で即時反映する。
    //
    // #588 TimedFire: ハートビートは専用のターン実装・専用配送を持たない。時刻が来たら scheduler は
    // 発火先ゲートウェイのループへ `TimedFire` イベントを 1 本流すだけ（`run_one_heartbeat`）で、以降の
    // ターン（配送・ロック・記録・継続）はそのループの**通常ルート**が回す。固有なのは「時間のトリガー＋
    // 渡すプロンプト」と「発火の記録（`heartbeat_log`）」だけ。受け口の解決は `AppState::timed_fire_router`
    // （per-agent→共有・#400 と同型）で行うので、scheduler へ Discord 送信ハンドルを渡す必要はなくなった。
    //
    // per-session 直列化ロック（`SessionLocks`）の唯一のインスタンスは `AppState` が
    // 持ち（#588 Stage 2・`AppState::session_locks`）、scheduler・各ゲートウェイの受信ループ
    // （Discord）・Nostr ランタイムはその `Arc` を clone して**同じ実体**を共有する。これで
    // 時間トリガーと通常メッセージ処理のターンが同一 session id 上で直列化される。
    //
    // live G（global kill-switch = `cfg.agent.heartbeat_enabled`）は scheduler が
    // **発火時に** `heartbeat_config_rx` から読む（hot-reload 追従・起動時スナップにしない。
    // さもないと後から G=false にしても止まらない退行が出る・設計 §4.2）。config 変更・
    // set_my_heartbeat（PR3）・schedule CRUD（PR4）・発火ターン完了は `scheduler_wake` で
    // rebuild を促す。
    {
        let scheduler_state = state.clone();
        tokio::spawn(async move {
            scheduler::run_scheduler(scheduler_state, heartbeat_config_rx).await;
        });
    }

    // #603 / #628 条件 A: 時刻発火の**起動時セルフチェック**を「descriptor 登録簿 ↔ sink 登録簿の
    // 双方向照合」へ集約する。型で配線は強制した（マネージャは router 無しでは構築できない）が、
    // ゲートウェイのループが実際に起動して受け口を登録するのは非同期（特に Nostr は spawn 後）。
    // **手書きの kind 列挙を持たない**: 両登録簿の kind 集合を突き合わせ、(a) sink はあるが
    // descriptor が無い＝発火先を parse できない配線バグ、(b) descriptor が「立ち上がるべき」
    // （`should_be_running` が env を引く）なのに受け口が 0＝時刻発火がどこにも届かない、を ERROR で
    // 知らせる（#602 の黙った全 skip を、コンパイルに加えて運用でも二重に検知する）。新 transport を
    // 足しても、この照合はその descriptor と sink を自動で拾う（手書きリストの更新漏れが起きない）。
    {
        let check_state = state.clone();
        // External V3 gateways are discovered from DB-backed descriptors and live registration.
        let mut configured_shared_kinds: std::collections::HashSet<&'static str> =
            std::collections::HashSet::new();
        // #925: gate socket があれば V3 レーン（extgate）は立ち上がる。`ExtgateFire::should_be_running`
        // がこれを見る（sink は下の serve_uds ブロックで register_shared する）。含めないと起動時
        // セルフチェックが extgate を見ない（should_be_running=false で sink 不在を正常と誤認）。
        if gate_socket.is_some() {
            configured_shared_kinds.insert(opencrab_extgate::EXTGATE_TIMED_FIRE_KIND);
        }
        tokio::spawn(async move {
            // ループの起動→受け口登録は非同期なので猶予を置く（Discord は同期登録だが Nostr は
            // spawn 後に登録するため）。
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let Ok(conn) = check_state.db.lock() else {
                tracing::error!("timed-fire: 起動時セルフチェックで db lock 取得に失敗");
                return;
            };
            let env = opencrab_actions::TransportFireEnv {
                conn: &conn,
                configured_shared_kinds: &configured_shared_kinds,
            };
            let issues = check_state.timed_fire_router.self_check(&env);
            if issues.is_empty() {
                tracing::info!(
                    "timed-fire: 起動時セルフチェック OK（descriptor ↔ sink 双方向照合・prefix 排他・受け口あり）"
                );
            }
            for issue in issues {
                match issue {
                    opencrab_actions::TimedFireSelfCheckIssue::SinkWithoutDescriptor { kind } => {
                        tracing::error!(
                            kind,
                            "timed-fire: sink はあるが descriptor が無い（発火先を parse できない＝配線バグ）"
                        );
                    }
                    opencrab_actions::TimedFireSelfCheckIssue::ExpectedSinkMissing { kind } => {
                        tracing::error!(
                            kind,
                            "timed-fire: 有効な受信ゲートウェイがあるのに受け口が 0（時刻発火が届かない）。配線/起動を確認"
                        );
                    }
                    opencrab_actions::TimedFireSelfCheckIssue::PrefixCollision {
                        owner,
                        shadowed_by,
                    } => {
                        tracing::error!(
                            owner,
                            shadowed_by,
                            "timed-fire: prefix 排他違反（2 つの transport が同じ session_id を parse する）。first-match で発火先が横取りされる。descriptor の parse 書式を分離せよ"
                        );
                    }
                }
            }
        });
    }

    let _watcher_handle = opencrab_server::hot_reload::start_config_watcher(
        "config",
        state.db.clone(),
        // #412: 「default_model が変わったか」の基準。稼働中の実効 spec そのものを渡す
        // （上の `format!("{provider}:{model}")` と同じ形でないと永久に不一致になる）。
        state.default_model.clone(),
        state.tools_config.clone(),
        heartbeat_config_tx,
    );

    _watcher_handle
}
