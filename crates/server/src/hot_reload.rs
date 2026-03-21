use std::path::Path;
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;

use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};

/// Start a background file watcher on the config directory.
///
/// When any file in the directory changes, the watcher re-parses the config
/// and live-updates the shared `ToolsConfig`.
pub fn start_config_watcher(
    config_dir: impl AsRef<Path>,
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
                        tracing::error!(
                            "Failed to parse config file {:?}: {}",
                            path,
                            e
                        );
                        continue;
                    }
                };

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
                    heartbeat_channel_id: cfg.gateway.discord.heartbeat_channel_id,
                };
                let _ = heartbeat_config_tx.send(hb_config);
            }
        }
    })
}
