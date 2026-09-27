//! Generic gate-admin Unix-socket client for daemon-owned reconciliation.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use zeroize::Zeroizing;

#[derive(Clone)]
pub struct GateAdminClient {
    socket: PathBuf,
    token: std::sync::Arc<Zeroizing<String>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ObservedBinding {
    pub binding_id: String,
    pub address: String,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ObservedInstance {
    pub instance_id: String,
    pub kind_id: String,
    pub subject_id: i64,
    pub revision: u64,
    pub enabled: bool,
    pub config_b64: String,
    pub config_digest: String,
    #[serde(default)]
    pub bindings: Vec<ObservedBinding>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DesiredInstance<'a> {
    pub kind_id: &'a str,
    pub subject_id: i64,
    pub enabled: bool,
    pub config_b64: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_grant: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct ReconcileDesired<'a> {
    pub instance_id: &'a str,
    pub kind_id: &'a str,
    pub subject_id: i64,
    pub enabled: bool,
    pub config_b64: &'a str,
    pub subject_grant: Option<&'a str>,
    pub addresses: &'a [String],
}

impl GateAdminClient {
    pub fn from_credential_file(socket: PathBuf, credential: &Path) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(credential)?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "credential symlink is forbidden"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            anyhow::ensure!(
                metadata.permissions().mode() & 0o777 == 0o600,
                "credential must be mode 0600"
            );
        }
        anyhow::ensure!(metadata.is_file(), "credential must be a regular file");
        let token = Zeroizing::new(std::fs::read_to_string(credential)?.trim().to_string());
        anyhow::ensure!(!token.is_empty(), "credential must be nonempty");
        Ok(Self {
            socket,
            token: std::sync::Arc::new(token),
        })
    }

    pub fn new_for_test(socket: PathBuf, token: String) -> Self {
        Self {
            socket,
            token: std::sync::Arc::new(Zeroizing::new(token)),
        }
    }

    pub async fn get_instance(&self, instance_id: &str) -> Result<Option<ObservedInstance>> {
        let response = self
            .request(
                "GET",
                &format!("/api/gate-instances/{instance_id}"),
                None,
                true,
            )
            .await?;
        if response.status == 404 {
            return Ok(None);
        }
        response.json()
    }

    pub async fn put_instance(
        &self,
        instance_id: &str,
        desired: &DesiredInstance<'_>,
    ) -> Result<ObservedInstance> {
        self.request(
            "PUT",
            &format!("/api/gate-instances/{instance_id}"),
            Some(serde_json::to_value(desired)?),
            false,
        )
        .await?
        .json()
    }

    pub async fn revise_instance(
        &self,
        instance_id: &str,
        expected_revision: u64,
        enabled: bool,
        config_b64: &str,
    ) -> Result<ObservedInstance> {
        self.request("POST", &format!("/api/gate-instances/{instance_id}/revisions"), Some(json!({"expected_revision":expected_revision,"enabled":enabled,"config_b64":config_b64})), false).await?.json()
    }

    pub async fn put_binding(
        &self,
        binding_id: &str,
        instance_id: &str,
        address: &str,
        session_id: &str,
        title: &str,
    ) -> Result<ObservedBinding> {
        self.request("PUT", &format!("/api/gate-bindings/{binding_id}"), Some(json!({"instance_id":instance_id,"address":address,"session":{"session_id":session_id,"title":title}})), false).await?.json()
    }

    pub async fn delete_binding(&self, binding_id: &str) -> Result<()> {
        self.request(
            "DELETE",
            &format!("/api/gate-bindings/{binding_id}"),
            None,
            false,
        )
        .await?;
        Ok(())
    }

    /// Converges one generic instance and its complete binding inventory without direct DB access.
    pub async fn reconcile(&self, desired: ReconcileDesired<'_>) -> Result<ObservedInstance> {
        let mut observed = match self.get_instance(desired.instance_id).await? {
            Some(value) => value,
            None => {
                self.put_instance(
                    desired.instance_id,
                    &DesiredInstance {
                        kind_id: desired.kind_id,
                        subject_id: desired.subject_id,
                        enabled: desired.enabled,
                        config_b64: desired.config_b64,
                        subject_grant: desired.subject_grant,
                    },
                )
                .await?
            }
        };
        if observed.kind_id != desired.kind_id || observed.subject_id != desired.subject_id {
            anyhow::bail!("instance identity conflict");
        }
        if observed.enabled != desired.enabled || observed.config_b64 != desired.config_b64 {
            observed = self
                .revise_instance(
                    desired.instance_id,
                    observed.revision,
                    desired.enabled,
                    desired.config_b64,
                )
                .await?;
        }
        let namespace = uuid::Uuid::parse_str(desired.instance_id)?;
        let mut wanted = std::collections::BTreeSet::new();
        for address in desired.addresses {
            if let Some(binding) = observed
                .bindings
                .iter()
                .find(|binding| binding.address == *address)
            {
                wanted.insert(binding.binding_id.clone());
                continue;
            }
            let binding_id = uuid::Uuid::new_v5(&namespace, address.as_bytes()).to_string();
            let session_id = format!("extgate-{binding_id}");
            self.put_binding(
                &binding_id,
                desired.instance_id,
                address,
                &session_id,
                "gateway session",
            )
            .await?;
            wanted.insert(binding_id);
        }
        for binding in &observed.bindings {
            if !wanted.contains(&binding.binding_id) {
                self.delete_binding(&binding.binding_id).await?;
            }
        }
        let verified = self
            .get_instance(desired.instance_id)
            .await?
            .context("instance disappeared after reconciliation")?;
        let actual: std::collections::BTreeSet<_> = verified
            .bindings
            .iter()
            .map(|binding| binding.binding_id.clone())
            .collect();
        anyhow::ensure!(actual == wanted, "binding inventory mismatch");
        anyhow::ensure!(
            verified.enabled == desired.enabled && verified.config_b64 == desired.config_b64,
            "instance verification mismatch"
        );
        Ok(verified)
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        allow_not_found: bool,
    ) -> Result<AdminResponse> {
        let encoded = body
            .map(|v| serde_json::to_vec(&v))
            .transpose()?
            .unwrap_or_default();
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connect gate admin {}", self.socket.display()))?;
        let head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", self.token.as_str(), encoded.len());
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&encoded).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        AdminResponse::parse(&response, allow_not_found)
    }
}

struct AdminResponse {
    status: u16,
    body: Vec<u8>,
}
impl AdminResponse {
    fn parse(bytes: &[u8], allow_not_found: bool) -> Result<Self> {
        let split = bytes
            .windows(4)
            .position(|v| v == b"\r\n\r\n")
            .context("invalid admin HTTP response")?;
        let header = std::str::from_utf8(&bytes[..split])?;
        let status = header
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .context("missing HTTP status")?
            .parse()?;
        let body = bytes[split + 4..].to_vec();
        if !(200..300).contains(&status) && !(allow_not_found && status == 404) {
            let code = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| v.get("code").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| format!("http_{status}"));
            anyhow::bail!("gate admin rejected request: {code}");
        }
        Ok(Self { status, body })
    }

    fn json<T: for<'de> Deserialize<'de>>(self) -> Result<T> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read_request(stream: &mut UnixStream) -> Vec<u8> {
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&chunk[..read]);
            if let Some(split) = request.windows(4).position(|value| value == b"\r\n\r\n") {
                let header = std::str::from_utf8(&request[..split]).unwrap();
                let length = header
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("Content-Length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap();
                if request.len() >= split + 4 + length {
                    return request;
                }
            }
        }
    }

    async fn respond(stream: &mut UnixStream, status: u16, body: &Value) {
        let body = serde_json::to_vec(body).unwrap();
        let reason = if status == 404 { "Not Found" } else { "OK" };
        stream
            .write_all(
                format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.write_all(&body).await.unwrap();
    }

    #[test]
    fn admin_response_allows_404_only_when_explicitly_requested() {
        let bytes = b"HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 26\r\n\r\n{\"code\":\"subject_unknown\"}";
        assert!(AdminResponse::parse(bytes, true).is_ok());
        let error = match AdminResponse::parse(bytes, false) {
            Ok(_) => panic!("404 must be rejected unless explicitly allowed"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("subject_unknown"), "{error}");
    }

    #[tokio::test]
    async fn reconcile_preserves_existing_binding_id_and_session_for_same_instance_address() {
        let unique = uuid::Uuid::new_v4().to_string();
        let socket = PathBuf::from(format!("/tmp/oc-ga-{}.sock", &unique[..8]));
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let instance_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let binding_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let address = "opaque-address";
        let observed = json!({
            "instance_id":instance_id,"kind_id":"synthetic","subject_id":7,"revision":1,
            "enabled":true,"config_b64":"e30=","config_digest":"digest",
            "bindings":[{"binding_id":binding_id,"address":address,"session_id":"existing-session"}]
        });
        let server = tokio::spawn(async move {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = String::from_utf8(read_request(&mut stream).await).unwrap();
                if request.starts_with("GET /api/gate-instances/") {
                    respond(&mut stream, 200, &observed).await;
                } else {
                    assert_eq!(index, 1);
                    respond(&mut stream, 409, &json!({"code":"binding_conflict"})).await;
                }
            }
        });
        let client = GateAdminClient::new_for_test(socket.clone(), "protected-token".into());
        let result = client
            .reconcile(ReconcileDesired {
                instance_id,
                kind_id: "synthetic",
                subject_id: 7,
                enabled: true,
                config_b64: "e30=",
                subject_grant: None,
                addresses: &[address.to_string()],
            })
            .await
            .expect("existing binding should be retained without a new PUT");
        assert_eq!(result.bindings[0].binding_id, binding_id);
        assert_eq!(
            result.bindings[0].session_id.as_deref(),
            Some("existing-session")
        );
        server.await.unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    #[tokio::test]
    async fn s5_gate_admin_client_reconciles_generic_instance_and_binding_over_protected_uds() {
        let unique = uuid::Uuid::new_v4().to_string();
        let socket = PathBuf::from(format!("/tmp/oc-ga-{}.sock", &unique[..8]));
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let instance_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let address = "opaque-address";
        let binding_id = uuid::Uuid::new_v5(
            &uuid::Uuid::parse_str(instance_id).unwrap(),
            address.as_bytes(),
        )
        .to_string();
        let observed = json!({
            "instance_id":instance_id,"kind_id":"synthetic","subject_id":7,"revision":1,
            "enabled":true,"config_b64":"e30=","config_digest":"digest","bindings":[]
        });
        let final_observed = json!({
            "instance_id":instance_id,"kind_id":"synthetic","subject_id":7,"revision":1,
            "enabled":true,"config_b64":"e30=","config_digest":"digest",
            "bindings":[{"binding_id":binding_id,"address":address,
                "session_id":format!("extgate-{binding_id}")}]
        });
        let server = tokio::spawn(async move {
            for (index, (status, body)) in [
                (404, json!({})),
                (200, observed),
                (
                    200,
                    json!({"binding_id":binding_id,"address":address,
                    "session_id":format!("extgate-{binding_id}")}),
                ),
                (200, final_observed),
            ]
            .into_iter()
            .enumerate()
            {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let request = String::from_utf8(request).unwrap();
                assert!(request.contains("Authorization: Bearer protected-token"));
                match index {
                    0 => assert!(request.starts_with("GET /api/gate-instances/")),
                    1 => {
                        assert!(request.starts_with("PUT /api/gate-instances/"));
                        assert!(request.contains("\"subject_grant\":\"grant\""));
                    }
                    2 => assert!(request.starts_with("PUT /api/gate-bindings/")),
                    3 => assert!(request.starts_with("GET /api/gate-instances/")),
                    _ => unreachable!(),
                }
                respond(&mut stream, status, &body).await;
            }
        });
        let client = GateAdminClient::new_for_test(socket.clone(), "protected-token".into());
        let addresses = vec![address.to_string()];
        let result = client
            .reconcile(ReconcileDesired {
                instance_id,
                kind_id: "synthetic",
                subject_id: 7,
                enabled: true,
                config_b64: "e30=",
                subject_grant: Some("grant"),
                addresses: &addresses,
            })
            .await
            .unwrap();
        assert_eq!(result.bindings.len(), 1);
        server.await.unwrap();
        std::fs::remove_file(socket).unwrap();
    }
}
