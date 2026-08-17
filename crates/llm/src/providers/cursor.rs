//! Cursor CLI（headless agent）を LLM プロバイダとして扱う。
//!
//! subprocess で `cursor-agent -p --output-format json -m <model> --force <prompt>`
//! を実行し、出力 JSON の `result` フィールドを応答本文として取り出す。ネイティブ
//! function calling が無いため、tool 定義はプロンプトに XML で載せる（codex と共通の
//! [`build_cli_prompt`] を使う）。
//!
//! プロンプトは **positional 引数**で渡す。cursor-agent の headless（`-p`）は positional
//! を主インターフェースにしており、positional 無し（stdin 待ち）だと入力終端を待って
//! ハングする既知の不具合がある（公式フォーラム報告）。codex は `-` で stdin を明示
//! 指定できるが cursor-agent にその契約は無いため、確実な positional を採る。
//!
//! コマンド名はインストールによりゆれる（`cursor-agent` / `agent` / `cursor`）ため
//! `binary_path` で設定可能にしている。認証は `CURSOR_API_KEY`（config の api_key を
//! 環境変数で渡す）か `cursor-agent login` 済みのアンビエント認証のどちらでも動く。

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;
use tracing::{debug, warn};

use super::codex::build_cli_prompt;
use crate::message::*;
use crate::traits::{LlmProvider, ModelInfo};

/// 既定のバイナリ名。インストーラが必ず作る安定名 `cursor-agent` を採用。
/// 環境に応じて `binary_path` で `cursor` / `agent` に変更できる。
const DEFAULT_CURSOR_PATH: &str = "cursor-agent";
const DEFAULT_MODEL: &str = "gpt-5";
const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// ダッシュボード表示用の既定モデル候補（ID, context_window）。
/// 実際に選べるモデルは `cursor-agent models` で変わるため、config の `models` で
/// 上書きするのが正確。ここはあくまで初期候補。
static DEFAULT_MODELS: &[(&str, u32)] = &[
    ("gpt-5", 400_000),
    ("sonnet-4.5", 200_000),
    ("opus-4.1", 200_000),
    ("auto", 200_000),
];

#[derive(Debug, Clone)]
pub struct CursorProvider {
    binary_path: String,
    default_model: String,
    working_dir: Option<String>,
    timeout: Duration,
    extra_models: Vec<(String, u32)>,
    /// 設定時に `CURSOR_API_KEY` として subprocess に渡す。None なら
    /// `cursor-agent login` 済みのアンビエント認証に任せる。
    api_key: Option<String>,
    /// テレメトリ用の表示名（既定は形式名 "cursor"）。ルーティングキーは
    /// router 登録時に別途決まる。
    name: String,
}

impl CursorProvider {
    pub fn new() -> Self {
        Self {
            binary_path: DEFAULT_CURSOR_PATH.to_string(),
            default_model: DEFAULT_MODEL.to_string(),
            working_dir: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            extra_models: Vec::new(),
            api_key: None,
            name: "cursor".to_string(),
        }
    }

    /// 表示名を上書きする（同じ形式の接続先を別名で登録するとき）。
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_binary_path(mut self, path: impl Into<String>) -> Self {
        let p = path.into();
        if !p.trim().is_empty() {
            self.binary_path = p;
        }
        self
    }

    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        let m = model.into();
        if !m.trim().is_empty() {
            self.default_model = m;
        }
        self
    }

    pub fn with_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        if secs > 0 {
            self.timeout = Duration::from_secs(secs);
        }
        self
    }

    pub fn with_extra_models(mut self, models: Vec<(String, u32)>) -> Self {
        self.extra_models = models;
        self
    }

    /// `CURSOR_API_KEY` を設定する。空文字は「未設定」として扱う。
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        let k = key.into();
        self.api_key = if k.trim().is_empty() { None } else { Some(k) };
        self
    }

    fn build_command(&self, model: &str, prompt: &str, agent_id: Option<&str>) -> Command {
        let mut cmd = Command::new(resolve_binary(&self.binary_path));
        // タイムアウト/ドロップ時に子プロセスを確実に kill（孤児 agent を残さない）。
        cmd.kill_on_drop(true);
        cmd.arg("-p") // print / headless
            .arg("--output-format")
            .arg("json")
            .arg("-m")
            .arg(model)
            // 非対話で承認待ちしないよう自動承認（ファイル編集等でハングさせない）。
            .arg("--force")
            // プロンプトは positional で渡す（stdin 待ちハング回避）。OpenCrab の
            // プロンプトは常に `[Available Tools]`/`[System]` 等で始まりオプション
            // （`-` 始まり）と衝突しない。
            .arg(prompt);

        if let Some(key) = &self.api_key {
            cmd.env("CURSOR_API_KEY", key);
        }

        // per-agent workspace を cwd にする（cursor-agent は cwd のリポジトリを対象にする）。
        let working_dir: Option<String> = agent_id
            .map(|id| format!("data/agents/{}/workspace", id))
            .or_else(|| self.working_dir.clone());
        if let Some(dir) = &working_dir {
            std::fs::create_dir_all(dir).ok();
            cmd.current_dir(dir);
        }

        cmd
    }
}

/// `binary_path` を spawn 用に解決する。ディレクトリ付き相対パス（例 `bin/cursor-agent`）
/// は、child の `current_dir` を per-agent workspace に切り替えると解決できなくなるため、
/// サーバー cwd 基準で絶対パス化しておく。単なるコマンド名（PATH 検索）や絶対パスは
/// そのまま返す。
fn resolve_binary(path: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if path.contains('/') && p.is_relative() {
        std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
    } else {
        p.to_path_buf()
    }
}

impl Default for CursorProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmProvider for CursorProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn available_models(&self) -> Result<Vec<ModelInfo>> {
        let mut models: Vec<ModelInfo> = DEFAULT_MODELS
            .iter()
            .map(|(id, ctx)| ModelInfo {
                id: id.to_string(),
                name: id.to_string(),
                context_window: *ctx,
                supports_function_calling: false,
                supports_vision: false,
            })
            .collect();
        for (id, ctx) in &self.extra_models {
            if !models.iter().any(|m| m.id == *id) {
                models.push(ModelInfo {
                    id: id.clone(),
                    name: id.clone(),
                    context_window: *ctx,
                    supports_function_calling: false,
                    supports_vision: false,
                });
            }
        }
        Ok(models)
    }

    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse> {
        let model = if request.model.is_empty() {
            &self.default_model
        } else {
            &request.model
        };
        debug!(model = %model, "Cursor CLI chat completion");

        let prompt = build_cli_prompt(&request);

        let mut cmd = self.build_command(model, &prompt, request.agent_id.as_deref());
        // stdin は不要（プロンプトは positional）。閉じておき agent が入力を待たない
        // ようにする。stdout/stderr は取り込む。
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let output = tokio::time::timeout(self.timeout, cmd.output())
            .await
            .map_err(|_| {
                anyhow::anyhow!("cursor-agent timed out after {}s", self.timeout.as_secs())
            })?
            .context("failed to run cursor-agent CLI")?;

        let response_text = resolve_cursor_output(
            &format!("{}", output.status),
            output.status.success(),
            &output.stdout,
            &output.stderr,
        )?;

        let content = if response_text.trim().is_empty() {
            None
        } else {
            Some(MessageContent::Text(response_text))
        };

        Ok(ChatResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model: model.to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content,
                    name: None,
                    function_call: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                finish_reason: Some(FinishReason::Stop),
            }],
            // cursor-agent の JSON はトークン usage を返さない（duration のみ）。
            usage: Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            created: chrono::Utc::now().timestamp(),
        })
    }

    fn supports_function_calling(&self) -> bool {
        false
    }

    async fn health_check(&self) -> Result<bool> {
        let output = Command::new(&self.binary_path)
            .arg("--version")
            .output()
            .await;
        Ok(output.map(|o| o.status.success()).unwrap_or(false))
    }
}

/// `cursor-agent --output-format json` の出力から応答本文（`result`）を決める。
///
/// codex 同様「非ゼロ終了/`is_error` でも本文があれば捨てずに使う」方針（エラーは
/// warn に残す。総失敗 → フォールバックより有用）。JSON としてパースできない出力
/// （`--output-format text` 等）は、成功なら stdout をそのまま本文にする。
fn resolve_cursor_output(
    exit_display: &str,
    success: bool,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<String> {
    let stdout_s = String::from_utf8_lossy(stdout);
    let stderr_s = String::from_utf8_lossy(stderr);

    match parse_result_json(&stdout_s) {
        Some((result, is_error)) => {
            let trimmed = result.trim();
            if trimmed.is_empty() {
                if !success || is_error {
                    anyhow::bail!(
                        "cursor-agent failed (exit {}): {}",
                        exit_display,
                        stderr_s.trim()
                    );
                }
                return Ok(String::new());
            }
            if !success || is_error {
                warn!(
                    exit = %exit_display,
                    stderr = %stderr_s.trim(),
                    "cursor-agent reported an error but produced a result; using the result"
                );
            }
            Ok(result)
        }
        None => {
            if !success {
                anyhow::bail!(
                    "cursor-agent failed (exit {}): {}{}",
                    exit_display,
                    stderr_s,
                    stdout_s
                );
            }
            Ok(stdout_s.to_string())
        }
    }
}

/// stdout から `"result"` を持つ JSON オブジェクトを拾う。
/// 出力全体（単一 JSON オブジェクト）を優先し、ダメなら末尾行から順に試す
/// （stream-json 混在や前後ノイズに耐える）。戻り値は (result, is_error)。
fn parse_result_json(stdout: &str) -> Option<(String, bool)> {
    let whole = stdout.trim();
    let candidates = std::iter::once(whole).chain(stdout.lines().rev());
    for candidate in candidates {
        let c = candidate.trim();
        if !c.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(c) {
            if let Some(result) = v.get("result").and_then(|r| r.as_str()) {
                let is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
                return Some((result.to_string(), is_error));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults() {
        let p = CursorProvider::new();
        assert_eq!(p.binary_path, "cursor-agent");
        assert_eq!(p.default_model, "gpt-5");
        assert!(p.working_dir.is_none());
        assert!(p.api_key.is_none());
        assert_eq!(p.timeout, Duration::from_secs(300));
    }

    #[test]
    fn test_builders() {
        let p = CursorProvider::new()
            .with_binary_path("cursor")
            .with_default_model("sonnet-4.5")
            .with_working_dir("/tmp/ws")
            .with_timeout_secs(120)
            .with_api_key("sk-cursor");
        assert_eq!(p.binary_path, "cursor");
        assert_eq!(p.default_model, "sonnet-4.5");
        assert_eq!(p.working_dir.as_deref(), Some("/tmp/ws"));
        assert_eq!(p.timeout, Duration::from_secs(120));
        assert_eq!(p.api_key.as_deref(), Some("sk-cursor"));
        // 空文字は既定を維持 / api_key は None
        let p2 = CursorProvider::new()
            .with_binary_path("")
            .with_api_key("  ");
        assert_eq!(p2.binary_path, "cursor-agent");
        assert!(p2.api_key.is_none());
    }

    #[test]
    fn test_available_models_includes_extra() {
        let p = CursorProvider::new().with_extra_models(vec![("custom-x".to_string(), 128_000)]);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let models = rt.block_on(p.available_models()).unwrap();
        assert!(models.iter().any(|m| m.id == "gpt-5"));
        assert!(models.iter().any(|m| m.id == "custom-x"));
    }

    #[test]
    fn test_resolve_success_extracts_result() {
        // マルチバイト（絵文字）を含む result も正しく取り出せること。
        // 生バイト文字列は ASCII 限定なので通常文字列を bytes 化して渡す。
        let json = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":10,"result":"Hello 👋","session_id":"s1"}"#;
        let out = resolve_cursor_output("exit status: 0", true, json.as_bytes(), b"").unwrap();
        assert_eq!(out, "Hello 👋");
    }

    #[test]
    fn test_resolve_nonzero_keeps_result() {
        // 非ゼロ終了でも result があれば捨てない
        let json = br#"{"type":"result","result":"partial answer"}"#;
        let out = resolve_cursor_output("exit status: 1", false, json, b"some warning").unwrap();
        assert_eq!(out, "partial answer");
    }

    #[test]
    fn test_resolve_is_error_with_empty_result_fails() {
        let json = br#"{"type":"result","is_error":true,"result":""}"#;
        let err = resolve_cursor_output("exit status: 1", false, json, b"real reason")
            .unwrap_err()
            .to_string();
        assert!(err.contains("real reason"), "{err}");
        assert!(err.contains("exit status: 1"), "{err}");
    }

    #[test]
    fn test_resolve_result_in_last_line_of_stream() {
        // stream-json 風に前段イベントがあり、最後の行が result オブジェクト
        let out = br#"{"type":"assistant","text":"thinking"}
{"type":"result","result":"final text","is_error":false}"#;
        let got = resolve_cursor_output("exit status: 0", true, out, b"").unwrap();
        assert_eq!(got, "final text");
    }

    #[test]
    fn test_resolve_non_json_success_falls_back_to_stdout() {
        // JSON でない（text 形式）出力は成功時 stdout をそのまま使う
        let out = resolve_cursor_output("exit status: 0", true, b"plain text answer", b"").unwrap();
        assert_eq!(out, "plain text answer");
        // 非ゼロ + 非JSON は失敗（stderr を握りつぶさない）
        let err = resolve_cursor_output("exit status: 2", false, b"", b"why it died")
            .unwrap_err()
            .to_string();
        assert!(err.contains("why it died"), "{err}");
    }
}
