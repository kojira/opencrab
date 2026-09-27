//! Protected Discord-local administration over a mode-0600 Unix socket.

use crate::store::DiscordStore;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

#[derive(Debug, Deserialize)]
struct Request {
    id: String,
    op: String,
    scope_instance_id: String,
    instance_id: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    subject_id: Option<i64>,
    #[serde(default)]
    config_b64: Option<String>,
    #[serde(default)]
    addresses: Option<Vec<String>>,
    #[serde(default)]
    credential: Option<String>,
    #[serde(default)]
    subject_grant: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Serialize)]
struct Response {
    id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub type AdminScope = Arc<std::collections::BTreeSet<String>>;

pub fn spawn(
    socket: PathBuf,
    scope: AdminScope,
    store: Arc<Mutex<DiscordStore>>,
    master_key: Arc<[u8; 32]>,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move { serve(&socket, scope, store, master_key).await })
}

pub async fn serve(
    socket: &Path,
    scope: AdminScope,
    store: Arc<Mutex<DiscordStore>>,
    master_key: Arc<[u8; 32]>,
) -> Result<()> {
    prepare_socket(socket)?;
    let listener = UnixListener::bind(socket)?;
    set_mode(socket)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let scope = scope.clone();
        let store = store.clone();
        let key = master_key.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(stream, scope, store, key).await {
                tracing::warn!(error = %error, "discord local admin request failed");
            }
        });
    }
}

async fn handle(
    stream: UnixStream,
    scope: AdminScope,
    store: Arc<Mutex<DiscordStore>>,
    key: Arc<[u8; 32]>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => apply(request, &scope, &store, &key),
            Err(_) => Response {
                id: String::new(),
                ok: false,
                result: None,
                error: Some("bad_request".into()),
            },
        };
        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        write.write_all(&encoded).await?;
    }
    Ok(())
}

fn apply(
    request: Request,
    scope: &std::collections::BTreeSet<String>,
    store: &Mutex<DiscordStore>,
    key: &[u8; 32],
) -> Response {
    let id = request.id.clone();
    let result = (|| -> Result<Value> {
        anyhow::ensure!(
            request.scope_instance_id == request.instance_id
                && scope.contains(&request.instance_id),
            "scope_denied"
        );
        let store = store
            .lock()
            .map_err(|_| anyhow::anyhow!("store_unavailable"))?;
        match request.op.as_str() {
            "get" => Ok(store.get(&request.instance_id)?.map_or(Value::Null, |row| {
                json!({
                    "instance_id": row.instance_id,
                    "agent_id": row.agent_id,
                    "subject_id": row.subject_id,
                    "enabled": row.enabled,
                    "desired_generation": row.desired_generation,
                    "lifecycle_state": row.lifecycle_state,
                    "credential_configured": !row.credential_envelope.is_empty(),
                })
            })),
            "upsert" => {
                let canonical_config = crate::config::canonicalize_config_b64(
                    request
                        .config_b64
                        .as_deref()
                        .context("config_b64 required")?,
                )?;
                let generation = store.upsert_desired(
                    &request.instance_id,
                    request.agent_id.as_deref().context("agent_id required")?,
                    request.subject_id.context("subject_id required")?,
                    &canonical_config,
                    request.addresses.as_deref().unwrap_or_default(),
                    request.credential.as_deref().unwrap_or_default(),
                    request.subject_grant.as_deref(),
                    request.enabled.context("enabled required")?,
                    key,
                )?;
                Ok(json!({"desired_generation": generation, "state": "pending"}))
            }
            _ => anyhow::bail!("unknown_operation"),
        }
    })();
    match result {
        Ok(value) => Response {
            id,
            ok: true,
            result: Some(value),
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

fn prepare_socket(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => anyhow::bail!("admin socket path conflict"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
    anyhow::ensure!(
        std::fs::symlink_metadata(path)?.file_type().is_socket(),
        "not a socket"
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_mode(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::os::unix::fs::PermissionsExt as _;

    #[tokio::test]
    async fn s5_discord_admin_is_exact_scoped_mode_0600_and_redacts_secret() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("admin.sock");
        let store = Arc::new(Mutex::new(
            DiscordStore::open(&temp.path().join("owner.db")).unwrap(),
        ));
        let inspect = store.clone();
        let scope = Arc::new(std::collections::BTreeSet::from([
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string(),
        ]));
        let task = spawn(socket.clone(), scope, store, Arc::new([4; 32]));
        while !socket.exists() {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let request = json!({"id":"1","op":"upsert","scope_instance_id":"other",
            "instance_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","agent_id":"a","subject_id":1,
            "config_b64":"e30=","addresses":[],"credential":"super-secret","enabled":true});
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        assert!(line.contains("scope_denied"));
        assert!(!line.contains("super-secret"));

        let unauthorized = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let request = json!({"id":"forged","op":"upsert","scope_instance_id":unauthorized,
            "instance_id":unauthorized,"agent_id":"b","subject_id":2,
            "config_b64":"e30=","addresses":[],"credential":"forged-secret","enabled":true});
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        assert!(line.contains("scope_denied"));

        let instance_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let secret_config = base64::engine::general_purpose::STANDARD.encode(
            br#"{"agent_id":"a","self_bot_id":"123456789012345678","token":"config-leak"}"#,
        );
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let request = json!({"id":"secret-config","op":"upsert","scope_instance_id":instance_id,
            "instance_id":instance_id,"agent_id":"a","subject_id":1,
            "config_b64":secret_config,"addresses":[],"credential":"super-secret","enabled":true});
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        assert!(line.contains("\"ok\":false"));
        assert!(inspect.lock().unwrap().get(instance_id).unwrap().is_none());

        let config = base64::engine::general_purpose::STANDARD
            .encode(br#"{"agent_id":"a","self_bot_id":"123456789012345678"}"#);
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let request = json!({"id":"2","op":"upsert","scope_instance_id":instance_id,
            "instance_id":instance_id,"agent_id":"a","subject_id":1,
            "config_b64":config,"addresses":[],"credential":"super-secret","enabled":true});
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        assert!(line.contains("\"ok\":true"));
        assert!(!line.contains("super-secret"));
        let row = inspect.lock().unwrap().get(instance_id).unwrap().unwrap();
        assert!(!row.credential_envelope.contains("super-secret"));
        assert_eq!(
            &*inspect
                .lock()
                .unwrap()
                .decrypt_credential(instance_id, &[4; 32])
                .unwrap(),
            b"super-secret"
        );
        task.abort();
    }
}
