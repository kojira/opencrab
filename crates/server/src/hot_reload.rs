use std::path::Path;
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;

use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};

/// 再読み込みした設定を適用してよいか検証する（#412）。
///
/// `[llm] default_provider` / `default_model` は `agents.model` が空のエージェント
/// 全員が使う実効モデル。`model_pricing` に `context_window` が無いモデルへ**差し替えると**
/// 文脈予算が黙って既定値へ落ちる。**落ちた設定は適用しない**: ここで Err を返した
/// リロードは丸ごと捨て、プロセスは旧設定のまま走り続ける。
///
/// **検証するのは spec が実際に変わったときだけ。** 方針は「設定する時に弾く」であって、
/// 変えていないものを理由に弾くのはそれより広い。ホットリロードは `default_model` を
/// 適用しない（`state.default_model` は起動時に一度組み立てられるきり）ので、無条件に
/// 検証すると**リロードが適用しない値を根拠に、モデルと無関係な tools の編集まで
/// 巻き添えで捨てる**ことになる。
///
/// `running_default_model` は稼働中の実効 spec（`AppState.default_model`）。
/// 組み立て方は `main.rs` 側と**同じ `format!("{provider}:{model}")`** で、片方だけ
/// 分離した形になっていると永久に不一致になる。ホットリロードでは更新されない値
/// なので、比較対象は常に「いまプロセスが実際に使っているモデル」になる。
fn validate_reloaded_config(
    db: &opencrab_db::Db,
    running_default_model: &str,
    cfg: &crate::config::AppConfig,
) -> Result<(), String> {
    let spec = format!("{}:{}", cfg.llm.default_provider, cfg.llm.default_model);
    if spec == running_default_model {
        return Ok(());
    }
    let conn = db.lock().map_err(|e| format!("db lock failed: {e}"))?;
    crate::process::ensure_model_context_window_registered(&conn, &spec)
}

/// Start a background file watcher on the config directory.
///
/// When any file in the directory changes, the watcher re-parses the config
/// and live-updates the shared `ToolsConfig`.
///
/// `running_default_model` は起動時に決まった実効モデル spec。ここで差し替えたい
/// わけではなく、**「変わったか」の基準**としてだけ使う（[`validate_reloaded_config`]）。
pub fn start_config_watcher(
    config_dir: impl AsRef<Path>,
    db: opencrab_db::Db,
    running_default_model: String,
    tools_config: Arc<RwLock<opencrab_actions::tools::ToolsConfig>>,
    heartbeat_config_tx: tokio::sync::watch::Sender<opencrab_core::heartbeat::HeartbeatConfig>,
) -> JoinHandle<()> {
    let config_dir = config_dir.as_ref().to_path_buf();

    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();

        let mut debouncer = match new_debouncer(std::time::Duration::from_millis(300), tx) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("Failed to create config file watcher: {}", e);
                return;
            }
        };

        if let Err(e) = debouncer
            .watcher()
            .watch(&config_dir, notify::RecursiveMode::Recursive)
        {
            tracing::error!("Failed to watch config directory {:?}: {}", config_dir, e);
            return;
        }

        tracing::info!("Config hot-reload watcher started on {:?}", config_dir);

        // Keep the debouncer alive by holding it in scope.
        let _debouncer = debouncer;

        for result in rx {
            let events = match result {
                Ok(events) => events,
                Err(e) => {
                    tracing::error!("Config watcher error: {:?}", e);
                    continue;
                }
            };

            for event in events {
                if event.kind != DebouncedEventKind::Any {
                    continue;
                }

                let path = &event.path;

                // Only process .toml files
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }

                tracing::info!("Config file changed: {:?}", path);

                let raw = match std::fs::read_to_string(path) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("Failed to read config file {:?}: {}", path, e);
                        continue;
                    }
                };

                let expanded = crate::config::expand_env_vars(&raw);

                let cfg: crate::config::AppConfig = match toml::from_str(&expanded) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("Failed to parse config file {:?}: {}", path, e);
                        continue;
                    }
                };

                // #412: 検証に落ちた設定は**一切適用しない**。tools も heartbeat も
                // 触らずに次のイベントを待つ（旧設定のまま動き続ける）。
                if let Err(e) = validate_reloaded_config(&db, &running_default_model, &cfg) {
                    tracing::error!(
                        "Config reload rejected for {:?}; keeping the previous config: {e}",
                        path
                    );
                    continue;
                }

                match tools_config.write() {
                    Ok(mut guard) => {
                        *guard = cfg.tools;
                        tracing::info!("Tools config hot-reloaded from {:?}", path);
                    }
                    Err(e) => {
                        tracing::error!("Failed to acquire write lock for tools_config: {}", e);
                    }
                }

                // Heartbeat設定もホットリロード
                let hb_config = opencrab_core::heartbeat::HeartbeatConfig {
                    interval_secs: cfg.agent.heartbeat_interval_secs,
                    enabled: cfg.agent.heartbeat_enabled,
                };
                let _ = heartbeat_config_tx.send(hb_config);
            }
        }
    })
}

/// 落ちた設定を適用しないこと、そして**変えていないものは理由にしないこと**（#412）。
#[cfg(test)]
mod reload_validation_tests {
    use super::validate_reloaded_config;

    /// 稼働中の実効 spec。`main.rs` と同じ組み立て方（`provider:model`）。
    const RUNNING: &str = "p1:m1";

    fn empty_db() -> opencrab_db::Db {
        opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap())
    }

    fn db_with(provider: &str, model: &str, window: Option<i32>) -> opencrab_db::Db {
        let db = empty_db();
        {
            let conn = db.lock().unwrap();
            opencrab_db::queries::upsert_model_pricing(
                &conn,
                &opencrab_db::queries::ModelPricingRow {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    input_price_per_1m: 0.0,
                    output_price_per_1m: 0.0,
                    context_window: window,
                },
            )
            .unwrap();
        }
        db
    }

    fn cfg(provider: &str, model: &str) -> crate::config::AppConfig {
        toml::from_str(&format!(
            "[llm]\ndefault_provider = \"{provider}\"\ndefault_model = \"{model}\"\n"
        ))
        .unwrap()
    }

    /// **本題**: default_model を変えていないリロードは、そのモデルが未登録でも通る。
    ///
    /// ホットリロードは `default_model` を適用しない。適用しない値を根拠に、
    /// モデルと無関係な tools の編集まで捨てるのは「設定する時に弾く」より広い。
    #[test]
    fn unchanged_default_model_passes_even_when_unregistered() {
        let db = empty_db();
        assert!(validate_reloaded_config(&db, RUNNING, &cfg("p1", "m1")).is_ok());
    }

    /// 変えたなら検証する。未登録への差し替えは拒否。
    #[test]
    fn changed_default_model_must_be_registered() {
        let db = db_with("p1", "m1", Some(200_000));
        let err = validate_reloaded_config(&db, RUNNING, &cfg("p1", "m2")).unwrap_err();
        assert!(err.contains("/api/llm/model-pricing"), "{err}");
    }

    /// 変えた先が登録済みなら通る。
    #[test]
    fn changed_default_model_passes_when_registered() {
        let db = db_with("p1", "m2", Some(200_000));
        assert!(validate_reloaded_config(&db, RUNNING, &cfg("p1", "m2")).is_ok());
    }

    /// provider だけ変えても「変わった」扱い（比較は spec 全体）。
    #[test]
    fn changed_provider_alone_is_also_validated() {
        let db = empty_db();
        assert!(validate_reloaded_config(&db, RUNNING, &cfg("p2", "m1")).is_err());
    }

    /// `model_pricing` が空でも、**差し替えないリロードは止まらない**。
    /// 登録前に config を触れなくなるのは方針より広い制約なので、そこは通す。
    #[test]
    fn empty_model_pricing_only_blocks_actual_changes() {
        let db = empty_db();
        assert!(validate_reloaded_config(&db, RUNNING, &cfg("p1", "m1")).is_ok());
        assert!(validate_reloaded_config(&db, RUNNING, &cfg("p1", "m2")).is_err());
    }
}

/// watcher 本体の振る舞い（#412）: 拒否したリロードが `tools_config` にも
/// heartbeat 通知にも**到達しない**こと。
///
/// 検証の単体テストだけでは「Err を返す」までしか押さえられず、呼び出し側が
/// その Err を無視していても気づけない。ここはファイルを実際に書いて watcher を
/// 通す。**先に正常な変更が適用されることを確認**してから異常系を見る（陽性対照が
/// 無いと、単にイベントが届いていないだけで「適用されなかった」と誤判定する）。
#[cfg(test)]
mod watcher_rejection_tests {
    use super::start_config_watcher;
    use std::path::Path;
    use std::sync::{Arc, RwLock};
    use std::time::{Duration, Instant};

    /// 稼働中の実効 spec。これと同じなら「変えていない」。
    const RUNNING: &str = "p1:m1";
    /// 正常な変更が反映されるまでの待ち上限。遅いマシンでも落ちないよう長めに取る。
    const APPLY_WAIT: Duration = Duration::from_secs(20);
    /// 拒否されたはずの変更が現れないことを見る観測窓。陽性対照が直前にこの経路の
    /// 実レイテンシを示しているので、同じ書き直しを数回繰り返せる長さがあれば足りる。
    const REJECT_WINDOW: Duration = Duration::from_secs(3);
    /// 書き直しの間隔。デバウンス（300ms）より長くしないと、書き直しがデバウンスを
    /// 延々とリセットして一度も発火しない。
    const REWRITE_INTERVAL: Duration = Duration::from_millis(500);

    fn config_text(provider: &str, model: &str, tools_enabled: bool, hb_secs: u64) -> String {
        format!(
            "[llm]\ndefault_provider = \"{provider}\"\ndefault_model = \"{model}\"\n\
             [tools]\nenabled = {tools_enabled}\n\
             [agent]\nheartbeat_interval_secs = {hb_secs}\n"
        )
    }

    /// `content` を書き直しながら `cond` の成立を最大 `limit` 待つ。戻り値は成立したか。
    ///
    /// watcher スレッドの起動と最初の書き込みが競合すると、イベントを 1 度取りこぼした
    /// まま二度と発火しない（CI で実際に踏んだ）。同じ内容でも書き直せば再びイベントに
    /// なるので、成立するまで書き直しながら待つ。異常系でも「拒否されるはずの内容」を
    /// 何度も与えることになり、観測の機会が増える方向にしか働かない。
    fn poll_with_rewrites(
        path: &Path,
        content: &str,
        limit: Duration,
        mut cond: impl FnMut() -> bool,
    ) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            std::fs::write(path, content).unwrap();
            let next_write = Instant::now() + REWRITE_INTERVAL;
            while Instant::now() < next_write {
                if cond() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if Instant::now() >= deadline {
                return cond();
            }
        }
    }

    #[test]
    fn rejected_reload_touches_neither_tools_nor_heartbeat() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.toml");
        std::fs::write(&path, config_text("p1", "m1", false, 60)).unwrap();

        // `model_pricing` は空。よって「変えたら拒否 / 変えなければ通る」の両方が出る。
        let db = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
        let tools_config = Arc::new(RwLock::new(opencrab_actions::tools::ToolsConfig::default()));
        let (hb_tx, hb_rx) =
            tokio::sync::watch::channel(opencrab_core::heartbeat::HeartbeatConfig {
                interval_secs: 60,
                enabled: false,
            });

        let _handle = start_config_watcher(
            dir.path(),
            db,
            RUNNING.to_string(),
            tools_config.clone(),
            hb_tx,
        );

        // 陽性対照: default_model を変えない編集は適用される（watcher が生きている証拠）。
        assert!(
            poll_with_rewrites(
                &path,
                &config_text("p1", "m1", true, 90),
                APPLY_WAIT,
                || { tools_config.read().unwrap().enabled && hb_rx.borrow().interval_secs == 90 }
            ),
            "default_model を変えない編集は適用されるはず（watcher が動いていない）"
        );

        // 本題: 未登録モデルへ差し替える編集は、tools も heartbeat も動かさない。
        assert!(
            !poll_with_rewrites(
                &path,
                &config_text("p1", "m2", false, 120),
                REJECT_WINDOW,
                || { !tools_config.read().unwrap().enabled || hb_rx.borrow().interval_secs == 120 }
            ),
            "拒否したリロードが tools_config / heartbeat のどちらかへ到達した"
        );
    }
}
