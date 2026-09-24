//! Protected Web-local administration over a mode-0600 Unix socket.

use crate::store::WebStore;
use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

#[derive(Deserialize)]
struct Request {
    id: String,
    op: String,
    scope_instance_id: String,
    instance_id: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    revision: Option<u64>,
    #[serde(default)]
    author_id: Option<String>,
    #[serde(default)]
    credential: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

pub fn spawn(
    socket: PathBuf,
    store: Arc<Mutex<WebStore>>,
    key: Arc<[u8; 32]>,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move { serve(&socket, store, key).await })
}

pub async fn serve(socket: &Path, store: Arc<Mutex<WebStore>>, key: Arc<[u8; 32]>) -> Result<()> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::symlink_metadata(socket) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(socket)?,
        Ok(_) => anyhow::bail!("admin socket path conflict"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(socket)?;
    set_mode(socket)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let store = store.clone();
        let key = key.clone();
        tokio::spawn(async move {
            let _ = handle(stream, store, key).await;
        });
    }
}

async fn handle(stream: UnixStream, store: Arc<Mutex<WebStore>>, key: Arc<[u8; 32]>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => apply(request, &store, &key),
            Err(_) => json!({"id":"","ok":false,"error":"bad_request"}),
        };
        write.write_all(format!("{response}\n").as_bytes()).await?;
    }
    Ok(())
}

fn apply(request: Request, store: &Mutex<WebStore>, key: &[u8; 32]) -> Value {
    let result =
        (|| -> Result<Value> {
            anyhow::ensure!(
                request.scope_instance_id == request.instance_id,
                "scope_denied"
            );
            let store = store
                .lock()
                .map_err(|_| anyhow::anyhow!("store_unavailable"))?;
            match request.op.as_str(){
            "get"=>Ok(store.get(&request.instance_id)?.map_or(Value::Null,|row|json!({
                "instance_id":row.instance_id,"agent_id":row.agent_id,"revision":row.revision,
                "author_id":row.author_id,"enabled":row.enabled,
                "credential_configured":row.credential_envelope.is_some()
            }))),
            "upsert"=>{store.upsert(&request.instance_id,
                request.agent_id.as_deref().context("agent_id required")?,
                request.revision.context("revision required")?,
                request.author_id.as_deref().context("author_id required")?,
                request.credential.as_deref(),request.enabled.context("enabled required")?,key)?;
                Ok(json!({"saved":true}))},
            _=>anyhow::bail!("unknown_operation"),
        }
        })();
    match result {
        Ok(value) => json!({"id":request.id,"ok":true,"result":value}),
        Err(error) => json!({"id":request.id,"ok":false,"error":error.to_string()}),
    }
}

#[cfg(unix)]
fn set_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
    anyhow::ensure!(
        std::fs::symlink_metadata(path)?.file_type().is_socket(),
        "not socket"
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
    use std::os::unix::fs::PermissionsExt as _;

    #[tokio::test]
    async fn s5_web_local_admin_is_scoped_redacted_and_rerunnable_without_server() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("admin.sock");
        let store = Arc::new(Mutex::new(
            WebStore::open(&temp.path().join("web.db")).unwrap(),
        ));
        let inspect = store.clone();
        let task = spawn(socket.clone(), store, Arc::new([3; 32]));
        while !socket.exists() {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let request = json!({"id":"1","op":"upsert","scope_instance_id":"other","instance_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","agent_id":"a","revision":1,"author_id":"author","credential":"secret-web","enabled":true});
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        assert!(line.contains("scope_denied"));
        assert!(!line.contains("secret-web"));

        let instance_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        for id in ["2", "3"] {
            let mut stream = UnixStream::connect(&socket).await.unwrap();
            let request = json!({"id":id,"op":"upsert","scope_instance_id":instance_id,
                "instance_id":instance_id,"agent_id":"a","revision":1,"author_id":"author",
                "credential":"secret-web","enabled":true});
            stream
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).await.unwrap();
            assert!(line.contains("\"ok\":true"));
            assert!(!line.contains("secret-web"));
        }
        let row = inspect.lock().unwrap().get(instance_id).unwrap().unwrap();
        assert!(!row.credential_envelope.unwrap().contains("secret-web"));
        task.abort();
    }
}
