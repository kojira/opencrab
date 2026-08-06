use std::path::Path;
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;

use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};

/// 再読み込みした設定を適用してよいか検証する（#412）。
///
/// `[llm] default_provider` / `default_model` は `agents.model` が空のエージェント
/// 全員が使う実効モデル。`model_pricing` に `context_window` が無いモデルへ差し替えると
/// 文脈予算が黙って既定値へ落ちる。**落ちた設定は適用しない**: ここで Err を返した
/// リロードは丸ごと捨て、プロセスは旧設定のまま走り続ける。
fn validate_reloaded_config(
    db: &opencrab_db::Db,
    cfg: &crate::config::AppConfig,
) -> Result<(), String> {
    let spec = format!("{}:{}", cfg.llm.default_provider, cfg.llm.default_model);
    let conn = db.lock().map_err(|e| format!("db lock failed: {e}"))?;
    crate::process::ensure_model_context_window_registered(&conn, &spec)
}

/// Start a background file watcher on the config directory.
///
/// When any file in the directory changes, the watcher re-parses the config
/// and live-updates the shared `ToolsConfig`.
pub fn start_config_watcher(
    config_dir: impl AsRef<Path>,
    db: opencrab_db::Db,
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
                if let Err(e) = validate_reloaded_config(&db, &cfg) {
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

/// 落ちた設定を適用しないこと（#412）。
#[cfg(test)]
mod reload_validation_tests {
    use super::validate_reloaded_config;

    fn db_with(provider: &str, model: &str, window: Option<i32>) -> opencrab_db::Db {
        let conn = opencrab_db::init_memory().unwrap();
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
        opencrab_db::Db::from_connection(conn)
    }

    fn cfg(provider: &str, model: &str) -> crate::config::AppConfig {
        toml::from_str(&format!(
            "[llm]\ndefault_provider = \"{provider}\"\ndefault_model = \"{model}\"\n"
        ))
        .unwrap()
    }

    #[test]
    fn registered_default_model_is_accepted() {
        let db = db_with("p1", "m1", Some(200_000));
        assert!(validate_reloaded_config(&db, &cfg("p1", "m1")).is_ok());
    }

    /// 未登録のモデルへ差し替える設定は拒否される。呼び出し側はこの Err で
    /// `continue` するので、tools も heartbeat も更新されない（旧設定が残る）。
    #[test]
    fn unregistered_default_model_is_rejected() {
        let db = db_with("p1", "m1", Some(200_000));
        let err = validate_reloaded_config(&db, &cfg("p1", "m2")).unwrap_err();
        assert!(err.contains("/api/llm/model-pricing"), "{err}");
    }

    /// 空の DB（= 本番の初期状態）では、登録前のリロードは通らない。
    #[test]
    fn empty_model_pricing_rejects_every_reload() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        assert!(validate_reloaded_config(&db, &cfg("p1", "m1")).is_err());
    }
}
