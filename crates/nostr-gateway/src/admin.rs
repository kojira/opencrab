//! Gateway-owned local admin interface. No server proxy and no secret echo.

#[cfg(unix)]
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use opencrab_nostr::MasterKey;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::store::{GatewayStore, InstanceRow};

const MAX_ADMIN_FRAME: usize = 65_536;

#[derive(Debug, Deserialize)]
struct Request {
    id: String,
    op: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    agent_name: Option<String>,
    #[serde(default)]
    secret_key: Option<String>,
    #[serde(default)]
    relays: Option<Vec<String>>,
    #[serde(default)]
    filter: Option<Value>,
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Debug, Serialize)]
struct Response {
    id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub fn spawn(
    socket: PathBuf,
    store: Arc<Mutex<GatewayStore>>,
    master_key: MasterKey,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move { serve(&socket, store, master_key).await })
}

async fn serve(
    socket: &Path,
    store: Arc<Mutex<GatewayStore>>,
    master_key: MasterKey,
) -> Result<()> {
    prepare_socket(socket)?;
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("bind gateway admin socket {}", socket.display()))?;
    set_socket_mode(socket)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let store = store.clone();
        let master_key = master_key.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, store, master_key).await {
                tracing::warn!(error = %format!("{error:#}"), "gateway admin connection failed");
            }
        });
    }
}

fn prepare_socket(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => anyhow::bail!(
            "admin socket path exists and is not a socket: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(unix)]
fn set_socket_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_socket_mode(_path: &Path) -> Result<()> {
    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    store: Arc<Mutex<GatewayStore>>,
    master_key: MasterKey,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let response = if line.len() > MAX_ADMIN_FRAME {
            Response {
                id: String::new(),
                ok: false,
                result: None,
                error: Some("frame_too_large".to_string()),
            }
        } else {
            match serde_json::from_str::<Request>(&line) {
                Ok(request) => apply(request, &store, &master_key),
                Err(_) => Response {
                    id: String::new(),
                    ok: false,
                    result: None,
                    error: Some("bad_request".to_string()),
                },
            }
        };
        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        write.write_all(&encoded).await?;
    }
    Ok(())
}

fn apply(request: Request, store: &Mutex<GatewayStore>, master_key: &MasterKey) -> Response {
    let id = request.id.clone();
    match apply_inner(request, store, master_key) {
        Ok(result) => Response {
            id,
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(error) => Response {
            id,
            ok: false,
            result: None,
            error: Some(error.to_string()),
        },
    }
}

fn apply_inner(
    request: Request,
    store: &Mutex<GatewayStore>,
    master_key: &MasterKey,
) -> Result<Value> {
    let store = store
        .lock()
        .map_err(|_| anyhow::anyhow!("gateway store lock poisoned"))?;
    match request.op.as_str() {
        "list" => {
            let rows = store.list_enabled()?;
            Ok(json!(rows.iter().map(public_row).collect::<Vec<_>>()))
        }
        "get" => {
            let agent_id = required(request.agent_id, "agent_id")?;
            Ok(store
                .get(&agent_id)?
                .map_or(Value::Null, |row| public_row(&row)))
        }
        "upsert" => {
            let agent_id = required(request.agent_id, "agent_id")?;
            validate_agent_id(&agent_id)?;
            let existing = store.get(&agent_id)?;
            let secret_key = request
                .secret_key
                .or_else(|| existing.as_ref().map(|row| row.secret_key.clone()))
                .filter(|value| !value.trim().is_empty())
                .context("secret_key is required for a new instance")?;
            let relays = request.relays.unwrap_or_default();
            for relay in &relays {
                let parsed = url::Url::parse(relay).context("relay URL is invalid")?;
                if !matches!(parsed.scheme(), "wss" | "ws") || parsed.host_str().is_none() {
                    anyhow::bail!("relay URL must use ws or wss and include a host");
                }
            }
            let filter = request.filter.unwrap_or_else(|| json!({}));
            if !filter.is_object() {
                anyhow::bail!("filter must be a JSON object");
            }
            store.upsert(
                &InstanceRow {
                    agent_id: agent_id.clone(),
                    agent_name: request
                        .agent_name
                        .or_else(|| existing.as_ref().map(|row| row.agent_name.clone()))
                        .unwrap_or_else(|| agent_id.clone()),
                    secret_key,
                    relays_json: serde_json::to_string(&relays)?,
                    filter_json: serde_json::to_string(&filter)?,
                    enabled: request
                        .enabled
                        .unwrap_or_else(|| existing.as_ref().is_some_and(|row| row.enabled)),
                },
                master_key,
            )?;
            Ok(public_row(
                &store.get(&agent_id)?.context("saved row missing")?,
            ))
        }
        "set_enabled" => {
            let agent_id = required(request.agent_id, "agent_id")?;
            let enabled = request.enabled.context("enabled is required")?;
            Ok(json!({"updated": store.set_enabled(&agent_id, enabled)?}))
        }
        "delete" => {
            let agent_id = required(request.agent_id, "agent_id")?;
            Ok(json!({"deleted": store.delete(&agent_id)?}))
        }
        _ => anyhow::bail!("unknown operation"),
    }
}

fn public_row(row: &InstanceRow) -> Value {
    json!({
        "agent_id": row.agent_id,
        "agent_name": row.agent_name,
        "relays": serde_json::from_str::<Value>(&row.relays_json).unwrap_or_else(|_| json!([])),
        "filter": serde_json::from_str::<Value>(&row.filter_json).unwrap_or_else(|_| json!({})),
        "enabled": row.enabled,
        "secret_configured": !row.secret_key.trim().is_empty(),
    })
}

fn required(value: Option<String>, name: &str) -> Result<String> {
    value
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("{name} is required"))
}

fn validate_agent_id(value: &str) -> Result<()> {
    if value.len() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        anyhow::bail!("agent_id contains unsupported characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroize::Zeroizing;

    fn key() -> MasterKey {
        Arc::new(Zeroizing::new([7; 32]))
    }

    #[test]
    fn admin_never_returns_secret_material() {
        let dir = tempfile::tempdir().unwrap();
        let store = Mutex::new(GatewayStore::open(&dir.path().join("gateway.db")).unwrap());
        let response = apply(
            Request {
                id: "1".to_string(),
                op: "upsert".to_string(),
                agent_id: Some("agent-1".to_string()),
                agent_name: Some("Agent One".to_string()),
                secret_key: Some("nsec1do-not-echo".to_string()),
                relays: Some(vec!["wss://relay.example".to_string()]),
                filter: Some(json!({})),
                enabled: Some(true),
            },
            &store,
            &key(),
        );
        let encoded = serde_json::to_string(&response).unwrap();
        assert!(response.ok);
        assert!(!encoded.contains("nsec1do-not-echo"));
        assert!(!encoded.contains("enc:v1"));
        assert!(encoded.contains("secret_configured"));
    }

    #[test]
    fn admin_rejects_unsafe_identifiers_and_relay_urls() {
        let dir = tempfile::tempdir().unwrap();
        let store = Mutex::new(GatewayStore::open(&dir.path().join("gateway.db")).unwrap());
        let invalid = |agent_id: &str, relay: &str| Request {
            id: "1".to_string(),
            op: "upsert".to_string(),
            agent_id: Some(agent_id.to_string()),
            agent_name: None,
            secret_key: Some("nsec1test".to_string()),
            relays: Some(vec![relay.to_string()]),
            filter: Some(json!({})),
            enabled: Some(false),
        };
        assert!(!apply(invalid("../escape", "wss://relay.example"), &store, &key()).ok);
        assert!(!apply(invalid("agent-1", "file:///tmp/socket"), &store, &key()).ok);
    }
}
