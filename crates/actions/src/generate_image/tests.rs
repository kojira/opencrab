use super::*;
use crate::bridge::BridgedExecutor;
use crate::dispatcher::ActionDispatcher;
use opencrab_core::engine::types::ActionExecutor;
use opencrab_llm::{GeneratedImage, ImageGenerationError, ImageGenerationRequest, ImageGenerator};
use std::sync::Mutex;

/// 1x1 の RGB PNG。
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xFF, 0xFF, 0x3F,
    0x00, 0x05, 0xFE, 0x02, 0xFE, 0xDC, 0xCC, 0x59, 0xE7, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// 呼ばれた依頼を記録し、決めた結果を返す生成器。
struct FakeGenerator {
    result: Result<GeneratedImage, ImageGenerationError>,
    seen: Mutex<Vec<ImageGenerationRequest>>,
}

impl FakeGenerator {
    fn ok(png: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            result: Ok(GeneratedImage {
                png_bytes: png.to_vec(),
                revised_prompt: Some("a calm red cat".to_string()),
                usage: None,
                request_id: Some("req-1".to_string()),
            }),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn err(e: ImageGenerationError) -> Arc<Self> {
        Arc::new(Self {
            result: Err(e),
            seen: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ImageGenerator for FakeGenerator {
    async fn generate_image(
        &self,
        request: &ImageGenerationRequest,
    ) -> Result<GeneratedImage, ImageGenerationError> {
        self.seen.lock().unwrap().push(request.clone());
        self.result.clone()
    }
}

fn context(root: &std::path::Path, caller: crate::CallerIdentity) -> ActionContext {
    let ws = opencrab_core::workspace::Workspace::from_root(root).unwrap();
    ActionContext {
        caller,
        agent_id: "agent-1".to_string(),
        agent_name: "Test Agent".to_string(),
        session_id: Some("session-1".to_string()),
        db: opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap()),
        workspace: Arc::new(ws),
        last_metrics_id: Arc::new(std::sync::Mutex::new(None)),
        model_override: Arc::new(std::sync::Mutex::new(None)),
        current_purpose: Arc::new(std::sync::Mutex::new(String::new())),
        runtime_info: Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
            default_model: "mock:test-model".to_string(),
            active_model: None,
            available_providers: vec![],
            gateway: "test".to_string(),
        })),
    }
}

fn executor_with(
    generator: Option<Arc<dyn ImageGenerator>>,
    root: &std::path::Path,
    caller: crate::CallerIdentity,
) -> BridgedExecutor {
    let mut dispatcher = ActionDispatcher::new();
    dispatcher.register(Arc::new(GenerateImageAction::new(generator)));
    BridgedExecutor::new(dispatcher, context(root, caller))
}

#[tokio::test]
async fn generate_image_writes_png_under_generated_images_and_shows_it() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeGenerator::ok(PNG_1X1);
    let executor = executor_with(Some(fake.clone()), dir.path(), crate::CallerIdentity::Owner);

    let result = executor
        .execute(
            "generate_image",
            &json!({"prompt": "a red cat", "transparent_background": true}),
        )
        .await;
    assert!(result.success, "{:?}", result.error);

    // 依頼は prompt と背景指定をそのまま渡す。
    let seen = fake.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![ImageGenerationRequest {
            prompt: "a red cat".to_string(),
            transparent_background: true,
        }]
    );

    // generated_images/<timestamp>-<id>.png に PNG がそのまま書かれる。
    let path = result.data["path"].as_str().expect("path").to_string();
    assert!(path.starts_with("generated_images/"), "{path}");
    assert!(path.ends_with(".png"), "{path}");
    let written = std::fs::read(dir.path().join(&path)).unwrap();
    assert_eq!(written, PNG_1X1);
    assert_eq!(result.data["size_bytes"], PNG_1X1.len());
    assert_eq!(result.data["revised_prompt"], "a calm red cat");

    // 次の LLM 呼び出しで見せる画像が載る。
    assert_eq!(result.images.len(), 1);
    assert!(result.images[0].starts_with("data:image/png;base64,"));

    // 結果本文（serialize したもの）に base64 は出ない。
    let text = serde_json::to_string(&result).unwrap();
    assert!(!text.contains("base64"), "{text}");
    assert!(!text.contains("iVBOR"), "{text}");
}

#[tokio::test]
async fn generate_image_does_not_overwrite_and_uses_distinct_names() {
    let dir = tempfile::tempdir().unwrap();
    let action = GenerateImageAction::new(Some(FakeGenerator::ok(PNG_1X1)));
    let ctx = context(dir.path(), crate::CallerIdentity::Owner);
    let a = action.execute(&json!({"prompt": "x"}), &ctx).await;
    let b = action.execute(&json!({"prompt": "x"}), &ctx).await;
    assert!(a.success && b.success);
    assert_ne!(
        a.data.as_ref().unwrap()["path"],
        b.data.as_ref().unwrap()["path"]
    );
}

#[tokio::test]
async fn generate_image_rejects_empty_prompt_without_calling_the_generator() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeGenerator::ok(PNG_1X1);
    let action = GenerateImageAction::new(Some(fake.clone()));
    let ctx = context(dir.path(), crate::CallerIdentity::Owner);
    for args in [json!({}), json!({"prompt": ""}), json!({"prompt": "   "})] {
        let r = action.execute(&args, &ctx).await;
        assert!(!r.success, "{args}");
        assert!(r.error.unwrap().contains("prompt"));
    }
    assert!(fake.seen.lock().unwrap().is_empty());
    assert!(!dir.path().join("generated_images").exists());
}

#[tokio::test]
async fn generate_image_without_configured_provider_is_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let action = GenerateImageAction::new(None);
    let ctx = context(dir.path(), crate::CallerIdentity::Owner);
    let r = action.execute(&json!({"prompt": "x"}), &ctx).await;
    assert!(!r.success);
    assert!(
        r.error.unwrap().contains("chatgpt"),
        "must name the missing provider"
    );
}

#[tokio::test]
async fn generate_image_surfaces_generator_errors() {
    let dir = tempfile::tempdir().unwrap();
    let action =
        GenerateImageAction::new(Some(FakeGenerator::err(ImageGenerationError::UsageLimit {
            message: "limit".into(),
            resets_at: Some(0),
        })));
    let ctx = context(dir.path(), crate::CallerIdentity::Owner);
    let r = action.execute(&json!({"prompt": "x"}), &ctx).await;
    assert!(!r.success);
    let err = r.error.unwrap();
    assert!(err.contains("usage limit"), "{err}");
    assert!(err.contains("1970-01-01"), "{err}");
}

#[tokio::test]
async fn generate_image_rejects_non_png_payload() {
    let dir = tempfile::tempdir().unwrap();
    let action = GenerateImageAction::new(Some(FakeGenerator::ok(b"not a png")));
    let ctx = context(dir.path(), crate::CallerIdentity::Owner);
    let r = action.execute(&json!({"prompt": "x"}), &ctx).await;
    assert!(!r.success);
    assert!(
        !dir.path().join("generated_images").exists()
            || std::fs::read_dir(dir.path().join("generated_images"))
                .unwrap()
                .next()
                .is_none()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn generate_image_refuses_symlinked_output_dir() {
    let outer = tempfile::tempdir().unwrap();
    let ws = outer.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let elsewhere = outer.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, ws.join("generated_images")).unwrap();
    let action = GenerateImageAction::new(Some(FakeGenerator::ok(PNG_1X1)));
    let ctx = context(&ws, crate::CallerIdentity::Owner);
    let r = action.execute(&json!({"prompt": "x"}), &ctx).await;
    assert!(!r.success);
    assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
}

/// 出力先が workspace 内の別ディレクトリを指す symlink でも拒否する（解決前の名前で判定する）。
#[cfg(unix)]
#[tokio::test]
async fn generate_image_refuses_output_dir_symlinked_inside_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let inside = dir.path().join("inside");
    std::fs::create_dir(&inside).unwrap();
    std::os::unix::fs::symlink(&inside, dir.path().join("generated_images")).unwrap();
    let action = GenerateImageAction::new(Some(FakeGenerator::ok(PNG_1X1)));
    let ctx = context(dir.path(), crate::CallerIdentity::Owner);
    let r = action.execute(&json!({"prompt": "x"}), &ctx).await;
    assert!(!r.success, "{:?}", r.data);
    assert!(std::fs::read_dir(&inside).unwrap().next().is_none());
}

/// 通常は背景 dispatch で走り `images` は結果に乗らないため、本文・説明は「見える」と言わず
/// `view_image` にそのパスを渡すよう案内する。
#[tokio::test]
async fn generate_image_result_text_points_to_view_image_without_claiming_visibility() {
    let dir = tempfile::tempdir().unwrap();
    let action = GenerateImageAction::new(Some(FakeGenerator::ok(PNG_1X1)));
    let ctx = context(dir.path(), crate::CallerIdentity::Owner);
    let r = action.execute(&json!({"prompt": "x"}), &ctx).await;
    assert!(r.success, "{:?}", r.error);
    let data = r.data.unwrap();
    let path = data["path"].as_str().unwrap();
    let message = data["message"].as_str().unwrap();
    assert!(message.contains(path), "{message}");
    assert!(message.contains("view_image"), "{message}");
    assert!(message.contains(&format!("path={path}")), "{message}");
    assert!(!message.contains("見える"), "{message}");

    let description = action.description();
    assert!(description.contains("view_image"), "{description}");
    assert!(!description.contains("見える"), "{description}");
}

// ---- 方針（owner 限定・背景 dispatch）----

#[test]
fn generate_image_is_owner_only_and_dispatchable() {
    assert!(crate::bridge::OWNER_ONLY_ACTIONS.contains(&"generate_image"));
    assert!(crate::tool_policy("generate_image").owner_only);
    assert!(crate::bridge::CORE_DISPATCHABLE_ACTIONS.contains(&"generate_image"));
    assert!(!crate::bridge::CORE_INLINE_ACTIONS.contains(&"generate_image"));
    assert!(!crate::default_non_dispatch_tools().contains("generate_image"));
    assert!(ActionDispatcher::new().has_action("generate_image"));
}

#[tokio::test]
async fn generate_image_is_refused_for_non_owner_callers() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeGenerator::ok(PNG_1X1);
    let executor = executor_with(Some(fake.clone()), dir.path(), crate::CallerIdentity::Agent);
    let r = executor
        .execute("generate_image", &json!({"prompt": "x"}))
        .await;
    assert!(!r.success);
    assert!(
        fake.seen.lock().unwrap().is_empty(),
        "quota must not be spent"
    );
    // 可視性は caller で絞らない（D-1056: tools をキャッシュ prefix に固定）。実行だけを止める。
}
