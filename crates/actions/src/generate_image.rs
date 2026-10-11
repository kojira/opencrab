//! `generate_image`: ChatGPT サブスクリプションで画像を生成してワークスペースへ保存する（D-1076）。
//!
//! 生成した PNG は `generated_images/<UTC時刻>-<短いID>.png` に新規作成で書く（上書きしない・
//! シンボリックリンクの出力先は拒否する）。結果にはワークスペース相対パスとバイト数だけを
//! 載せ、画像そのものは `ActionResult.images`（`view_image` と同じ経路）で次の LLM 呼び出しに
//! 見せる。base64 は結果本文にもログにも出さない。

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use opencrab_llm::image_input::{normalize_image_bytes, NormalizedImage};
use opencrab_llm::{ImageGenerationRequest, ImageGenerator};
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult};

/// 保存する画像の上限（32 MiB）。
pub const MAX_GENERATED_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// 出力先ディレクトリ（ワークスペース相対）。
const OUTPUT_DIR: &str = "generated_images";
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// 画像生成アクション。生成器は server が設定済みの chatgpt プロバイダから注入する
/// （`None` なら実行時に「未設定」エラーを返す。他プロバイダへのフォールバックは無い）。
pub struct GenerateImageAction {
    generator: Option<Arc<dyn ImageGenerator>>,
}

impl GenerateImageAction {
    pub fn new(generator: Option<Arc<dyn ImageGenerator>>) -> Self {
        Self { generator }
    }
}

/// 保存済みの画像。
struct SavedImage {
    relative_path: String,
    /// 次の LLM 呼び出しで見せる縮小版。正規化の上限（20 MiB）を超える・デコードできない
    /// 場合は `None`（ファイルは保存する）。
    shown: Option<NormalizedImage>,
}

/// PNG を検査・正規化してから `generated_images/` へ新規作成で書く。
fn save_generated_png(
    ws: &opencrab_core::workspace::Workspace,
    png: &[u8],
) -> anyhow::Result<SavedImage> {
    anyhow::ensure!(
        png.len() <= MAX_GENERATED_IMAGE_BYTES,
        "generated image too large ({} bytes, max {} bytes)",
        png.len(),
        MAX_GENERATED_IMAGE_BYTES
    );
    anyhow::ensure!(
        png.starts_with(PNG_SIGNATURE),
        "generated data is not a PNG"
    );
    let shown = match normalize_image_bytes(png) {
        Ok(img) => Some(img),
        Err(e) => {
            tracing::warn!(error = %e, "generate_image: preview skipped");
            None
        }
    };

    let dir = ws.resolve_path(OUTPUT_DIR)?;
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) => anyhow::ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "{OUTPUT_DIR} exists but is not a regular directory"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&dir)?,
        Err(e) => return Err(e.into()),
    }

    let id = uuid::Uuid::new_v4().simple().to_string();
    let name = format!(
        "{}-{}.png",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        &id[..8]
    );
    let relative_path = format!("{OUTPUT_DIR}/{name}");
    let path = Path::new(&dir).join(&name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(png)?;
    file.sync_all()?;
    Ok(SavedImage {
        relative_path,
        shown,
    })
}

#[async_trait]
impl Action for GenerateImageAction {
    fn name(&self) -> &str {
        "generate_image"
    }

    fn description(&self) -> &str {
        "画像を生成してワークスペースに PNG で保存する（ChatGPT サブスクリプション）。\
         生成には1〜2分かかる。完了すると保存先の相対パスが返り、その画像は次の応答から見える。\
         ユーザーに渡すときは既存の投稿・添付ツールでそのファイルを使う。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "生成したい画像の説明（空不可）"
                },
                "transparent_background": {
                    "type": "boolean",
                    "description": "背景を透過にする（既定 false）"
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let Some(prompt) = args["prompt"]
            .as_str()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            return ActionResult::error("prompt is required and must not be empty");
        };
        let Some(generator) = self.generator.as_ref() else {
            return ActionResult::error(
                "generate_image is unavailable: no chatgpt provider is configured",
            );
        };
        let request = ImageGenerationRequest {
            prompt: prompt.to_string(),
            transparent_background: args["transparent_background"].as_bool().unwrap_or(false),
        };
        let generated = match generator.generate_image(&request).await {
            Ok(img) => img,
            Err(e) => return ActionResult::error(&format!("generate_image failed: {e}")),
        };
        let size_bytes = generated.png_bytes.len();
        let ws = ctx.workspace.clone();
        let png = generated.png_bytes;
        let saved = match tokio::task::spawn_blocking(move || save_generated_png(&ws, &png)).await {
            Ok(Ok(saved)) => saved,
            Ok(Err(e)) => {
                return ActionResult::error(&format!("generate_image could not save: {e:#}"))
            }
            Err(e) => return ActionResult::error(&format!("generate_image save task failed: {e}")),
        };
        tracing::info!(
            agent_id = %ctx.agent_id,
            path = %saved.relative_path,
            size_bytes,
            request_id = generated.request_id.as_deref().unwrap_or(""),
            "generate_image saved"
        );
        let preview_note = if saved.shown.is_some() {
            "この画像は次の応答から見える。"
        } else {
            "画像が大きすぎるため次の応答には添付しない（必要なら view_image で確認する）。"
        };
        let mut data = json!({
            "path": saved.relative_path,
            "size_bytes": size_bytes,
            "message": format!(
                "画像を {} に保存した（{} bytes）。{preview_note}\
                 ユーザーに渡すときは既存の投稿・添付ツールでこのファイルを使う。",
                saved.relative_path, size_bytes
            ),
        });
        if let Some(revised) = generated.revised_prompt {
            data["revised_prompt"] = json!(revised);
        }
        let mut result = ActionResult::success(data);
        result.images = saved.shown.map(|img| img.data_url).into_iter().collect();
        result
    }
}

#[cfg(test)]
#[path = "generate_image/tests.rs"]
mod tests;
