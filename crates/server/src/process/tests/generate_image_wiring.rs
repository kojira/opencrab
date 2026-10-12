//! D-1076: ターンの executor（`build_turn_executor`）が、`reload_router` で組んだ router の
//! 画像生成器で `generate_image` を登録することを固定する。HTTP はローカルのモックだけを叩く。

use std::sync::Arc;

use base64::Engine as _;

use super::wiring::{build_turn_executor, TurnExecutorWiring};
use crate::config::{LlmConfig, ProviderConfig};
use opencrab_core::engine::types::ActionExecutor;

/// 1x1 PNG の base64（`data[0].b64_json` に入れる）。
const PNG_1X1_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVQI12P4//8/AAX+Av7czFnnAAAAAElFTkSuQmCC";
const NO_PROVIDER_ERROR: &str = "no chatgpt provider is configured";

/// 失効していない、`chatgpt_account_id` 入りの JWT（署名は検証されない）。
fn fake_access_token() -> String {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let exp = chrono::Utc::now().timestamp() + 3600;
    let payload = serde_json::json!({
        "exp": exp,
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-test"},
    });
    format!(
        "{}.{}.sig",
        b64.encode(br#"{"alg":"none"}"#),
        b64.encode(payload.to_string())
    )
}

/// 画像生成の成功応答を返し続け、受けた request line を記録する HTTP モック。
async fn spawn_imagegen_mock() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..pos]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if raw.len() >= pos + 4 + len {
                        break;
                    }
                }
            }
            let head = String::from_utf8_lossy(&raw).to_string();
            log.lock()
                .unwrap()
                .push(head.lines().next().unwrap_or_default().to_string());
            let body = serde_json::json!({
                "data": [{"b64_json": PNG_1X1_B64}],
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (format!("http://{addr}/backend-api"), seen)
}

fn owner_context(
    state: &crate::AppState,
    workspace_root: &std::path::Path,
) -> opencrab_actions::ActionContext {
    let workspace = opencrab_core::workspace::Workspace::from_root(workspace_root).unwrap();
    opencrab_actions::ActionContext {
        agent_id: "agent-1".to_string(),
        agent_name: "Agent".to_string(),
        session_id: Some("session-1".to_string()),
        db: state.db.clone(),
        workspace: Arc::new(workspace),
        last_metrics_id: Arc::new(std::sync::Mutex::new(None)),
        model_override: Arc::new(std::sync::Mutex::new(None)),
        current_purpose: Arc::new(std::sync::Mutex::new(String::new())),
        caller: opencrab_actions::CallerIdentity::Owner,
        runtime_info: Arc::new(std::sync::Mutex::new(opencrab_actions::RuntimeInfo {
            default_model: "mock:test".to_string(),
            active_model: None,
            available_providers: vec![],
            gateway: "test".to_string(),
        })),
    }
}

/// `llm_config` を差し替えて `reload_router` し、そのターン用 executor を組む。
fn turn_executor(
    llm_config: LlmConfig,
    workspace_root: &std::path::Path,
) -> opencrab_actions::BridgedExecutor {
    let mut state = crate::test_app_state();
    state.llm_config = Arc::new(llm_config);
    crate::api::providers::reload_router(&state).expect("reload_router");
    let context = owner_context(&state, workspace_root);
    build_turn_executor(
        &state,
        TurnExecutorWiring {
            context,
            depth: 0,
            gateway_actions: None,
            subtask_registry: Arc::new(dashmap::DashMap::new()),
            completion_sink: None,
            reply_target: None,
            tool_allowlist: None,
        },
        |_| None,
    )
}

#[tokio::test]
async fn turn_executor_wires_generate_image_to_the_chatgpt_provider() {
    let dir = tempfile::tempdir().unwrap();
    let auth_file = dir.path().join("auth.json");
    std::fs::write(
        &auth_file,
        serde_json::json!({
            "tokens": {"access_token": fake_access_token(), "refresh_token": "rt"},
        })
        .to_string(),
    )
    .unwrap();
    let (base_url, seen) = spawn_imagegen_mock().await;
    let mut llm_config = LlmConfig {
        default_provider: "sub".to_string(),
        ..Default::default()
    };
    llm_config.providers.insert(
        "sub".to_string(),
        ProviderConfig {
            provider_type: "chatgpt".to_string(),
            auth_file: auth_file.to_string_lossy().to_string(),
            base_url,
            ..Default::default()
        },
    );
    let workspace = dir.path().join("ws");
    let executor = turn_executor(llm_config, &workspace);

    let result = executor
        .execute(
            "generate_image",
            &serde_json::json!({"prompt": "a red cat"}),
        )
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        ["POST /backend-api/codex/images/generations HTTP/1.1"]
    );
    let path = result.data["path"].as_str().unwrap().to_string();
    assert!(workspace.join(path).is_file());
}

#[tokio::test]
async fn turn_executor_without_chatgpt_provider_reports_generate_image_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let mut llm_config = LlmConfig {
        default_provider: "openai".to_string(),
        ..Default::default()
    };
    llm_config.providers.insert(
        "openai".to_string(),
        ProviderConfig {
            api_key: "x".to_string(),
            ..Default::default()
        },
    );
    let executor = turn_executor(llm_config, dir.path());

    let result = executor
        .execute(
            "generate_image",
            &serde_json::json!({"prompt": "a red cat"}),
        )
        .await;
    assert!(!result.success);
    let error = result.error.unwrap();
    assert!(error.contains(NO_PROVIDER_ERROR), "{error}");
}
