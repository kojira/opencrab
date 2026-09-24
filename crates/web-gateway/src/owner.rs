//! Independently runnable Web owner process backed by the Web store.

use crate::store::WebStore;
use crate::v3::client::InstanceClient;
use crate::v3::http::{router, HttpState};
use crate::v3::wire::config_digest;
use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const MASTER_KEY_ENV: &str = "OPENCRAB_WEB_MASTER_KEY";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerConfig {
    pub database_path: PathBuf,
    pub admin_socket: PathBuf,
}

impl OwnerConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let value: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        for (name, path) in [
            ("database_path", &value.database_path),
            ("admin_socket", &value.admin_socket),
        ] {
            anyhow::ensure!(path.is_absolute(), "{name} must be absolute");
        }
        Ok(value)
    }
}

pub async fn run(config: OwnerConfig) -> Result<()> {
    let _lock = opencrab_process_supervisor::lock::StoreLock::acquire(
        &config.database_path.with_extension("lock"),
    )
    .context("another Web owner owns this store")?;
    let encoded = std::env::var(MASTER_KEY_ENV).context("web master key required")?;
    std::env::remove_var(MASTER_KEY_ENV);
    let key = crate::secret_store::parse_master_key(&encoded)?;
    let mut raw = [0_u8; 32];
    raw.copy_from_slice(&key[..]);
    let store = Arc::new(Mutex::new(WebStore::open(&config.database_path)?));
    let (bind, socket, rows) = {
        let locked = store
            .lock()
            .map_err(|_| anyhow::anyhow!("store unavailable"))?;
        let (bind, socket) = locked.settings()?;
        (bind, socket, locked.list_enabled()?)
    };
    let _admin = crate::admin::spawn(config.admin_socket, store, Arc::new(raw));
    let mut instances = Vec::new();
    let mut agent_clients = std::collections::HashMap::new();
    for row in rows {
        let digest = config_digest(&row.author_id);
        let client = InstanceClient::spawn(
            PathBuf::from(&socket),
            row.instance_id,
            row.revision,
            row.author_id,
            digest,
        );
        anyhow::ensure!(
            agent_clients.insert(row.agent_id, client.clone()).is_none(),
            "duplicate agent_id"
        );
        instances.push(client);
    }
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    axum::serve(
        listener,
        router(HttpState {
            instances,
            agent_clients,
        }),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn s5_web_owner_config_rejects_core_and_legacy_database_paths() {
        for forbidden in ["core_database_path", "legacy_database_path"] {
            let mut value =
                serde_json::json!({"database_path":"/tmp/web.db","admin_socket":"/tmp/web.sock"});
            value
                .as_object_mut()
                .unwrap()
                .insert(forbidden.into(), serde_json::json!("/tmp/core.db"));
            assert!(serde_json::from_value::<OwnerConfig>(value).is_err());
        }
    }

    #[tokio::test]
    async fn s5_web_owner_runs_without_server_and_refuses_a_second_store_owner() {
        let temp = tempfile::tempdir().unwrap();
        let database_path = temp.path().join("web.db");
        let admin_socket = temp.path().join("web-admin.sock");
        let store = WebStore::open(&database_path).unwrap();
        store
            .configure("127.0.0.1:0", "/tmp/stopped-core.sock")
            .unwrap();
        drop(store);
        let config = OwnerConfig {
            database_path,
            admin_socket: admin_socket.clone(),
        };
        let key = base64::engine::general_purpose::STANDARD.encode([7_u8; 32]);
        std::env::set_var(MASTER_KEY_ENV, &key);
        let owner = tokio::spawn(run(config.clone()));
        for _ in 0..100 {
            if admin_socket.exists() || owner.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(admin_socket.exists());
        assert!(!owner.is_finished());
        assert!(std::env::var(MASTER_KEY_ENV).is_err());

        std::env::set_var(MASTER_KEY_ENV, &key);
        let collision = run(config).await.unwrap_err();
        std::env::remove_var(MASTER_KEY_ENV);
        assert!(collision.to_string().contains("another Web owner"));
        owner.abort();
        let _ = owner.await;
    }
}
