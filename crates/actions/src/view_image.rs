//! `view_image`: 画像を次の LLM 呼び出しで見せる（D-1060）。
//!
//! `url`（https）かワークスペース内の `path` を 1 つ受け取り、長辺 1568px 以内へ縮めた
//! data URL を `ActionResult.images` に載せる。engine がそれを次の request にだけ添付する。

use async_trait::async_trait;
use opencrab_llm::image_input::{fetch_image_data_url, normalize_image_bytes, NormalizedImage};
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult};

pub struct ViewImageAction;

fn read_workspace_image(
    ws: &opencrab_core::workspace::Workspace,
    path: &str,
) -> anyhow::Result<NormalizedImage> {
    let resolved = ws.resolve_path(path)?;
    let meta = std::fs::metadata(&resolved)?;
    anyhow::ensure!(meta.is_file(), "not a file: {path}");
    anyhow::ensure!(
        meta.len() <= opencrab_llm::image_input::MAX_SOURCE_BYTES as u64,
        "image too large ({} bytes, max 20MB)",
        meta.len()
    );
    normalize_image_bytes(&std::fs::read(&resolved)?)
}

#[async_trait]
impl Action for ViewImageAction {
    fn name(&self) -> &str {
        "view_image"
    }

    fn description(&self) -> &str {
        "画像を見る。url（https）かワークスペース内の path を1つ渡すと、次の応答からこのターンの終わりまでその画像が見える。\
         大きい画像は長辺1568pxに縮小される。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "画像の https URL（path とどちらか1つ）"
                },
                "path": {
                    "type": "string",
                    "description": "画像ファイルのパス（ワークスペースルートからの相対パス。url とどちらか1つ）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let url = args["url"].as_str().filter(|s| !s.trim().is_empty());
        let path = args["path"].as_str().filter(|s| !s.trim().is_empty());
        let (viewed, outcome) = match (url, path) {
            (Some(url), None) => {
                if !url.trim().starts_with("https://") {
                    return ActionResult::error("url must start with https://");
                }
                (url.to_string(), fetch_image_data_url(url).await)
            }
            (None, Some(path)) => {
                let ws = ctx.workspace.clone();
                let owned = path.to_string();
                let read =
                    tokio::task::spawn_blocking(move || read_workspace_image(&ws, &owned)).await;
                let outcome = match read {
                    Ok(r) => r,
                    Err(e) => Err(anyhow::anyhow!("read task failed: {e}")),
                };
                (path.to_string(), outcome)
            }
            _ => return ActionResult::error("pass exactly one of url or path"),
        };
        match outcome {
            Ok(img) => {
                let mut result = ActionResult::success(json!({
                    "viewed": viewed,
                    "original": format!("{}x{}", img.orig_w, img.orig_h),
                    "sent": format!("{}x{}", img.w, img.h),
                    "resized": img.resized,
                }));
                result.images = vec![img.data_url];
                result
            }
            Err(e) => ActionResult::error(&format!("view_image failed for {viewed}: {e:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::BridgedExecutor;
    use crate::dispatcher::ActionDispatcher;
    use opencrab_core::engine::types::ActionExecutor;

    /// 1x1 の RGB PNG。
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xFF, 0xFF, 0x3F, 0x00, 0x05, 0xFE, 0x02, 0xFE, 0xDC, 0xCC, 0x59, 0xE7, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn context(root: &std::path::Path) -> ActionContext {
        let ws = opencrab_core::workspace::Workspace::from_root(root).unwrap();
        ActionContext {
            caller: crate::CallerIdentity::Agent,
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap()),
            workspace: std::sync::Arc::new(ws),
            last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_purpose: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            runtime_info: std::sync::Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test-model".to_string(),
                active_model: None,
                available_providers: vec![],
                gateway: "test".to_string(),
            })),
        }
    }

    #[tokio::test]
    async fn view_image_path_returns_data_url_image() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), PNG_1X1).unwrap();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), context(dir.path()));
        let result = executor
            .execute("view_image", &json!({"path": "a.png"}))
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.images.len(), 1);
        assert!(result.images[0].starts_with("data:image/png;base64,"));
        assert_eq!(result.data["sent"], "1x1");
        assert_eq!(result.data["resized"], false);
        assert!(!serde_json::to_string(&result).unwrap().contains("base64"));
    }

    #[tokio::test]
    async fn view_image_rejects_path_outside_workspace() {
        let outer = tempfile::tempdir().unwrap();
        std::fs::write(outer.path().join("secret.png"), PNG_1X1).unwrap();
        let ws = outer.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), context(&ws));
        let result = executor
            .execute("view_image", &json!({"path": "../secret.png"}))
            .await;
        assert!(!result.success);
        assert!(result.images.is_empty());
        assert!(result.error.unwrap().contains("view_image failed"));
    }

    #[tokio::test]
    async fn view_image_requires_exactly_one_https_source() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let both = ViewImageAction
            .execute(&json!({"path": "a.png", "url": "https://x/a.png"}), &ctx)
            .await;
        assert!(!both.success);
        let http = ViewImageAction
            .execute(&json!({"url": "http://x/a.png"}), &ctx)
            .await;
        assert!(!http.success);
    }
}
