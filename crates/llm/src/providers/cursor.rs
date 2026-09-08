//! Cursor CLI（headless agent）を LLM プロバイダとして扱う。
//!
//! subprocess で
//! `cursor-agent -p --output-format json --model <model> --plan --sandbox enabled --trust <prompt>`
//! を実行し、出力 JSON の `result` フィールドを応答本文として取り出す。ネイティブ
//! function calling が無いため、tool 定義はプロンプトに XML で載せる（codex と共通の
//! [`build_cli_prompt`] を使う）。
//!
//! **推論専用の頭として使う**（#674 / #682）。cursor-agent 自身は `-p`（headless）だと
//! write / shell を含む全ツールにアクセスできてしまう（`--help` に明記）。これは
//! opencrab の権限統制（#330）を素通りするため、cursor-agent 自身の write/shell を封じ、
//! read を注入済み XML `<function_calls>` へ誘導して、実行は opencrab 側の権限ゲート込み
//! ツールループに乗せる。効いている防御は次の 2 つ:
//!
//! 1. **deny cli.json（#682）**: 一時 cwd に `.cursor/cli.json` を置き
//!    Read/Write/Shell/WebFetch/WebSearch/Mcp を deny する（[`CURSOR_DENY_CONFIG`]）。
//!    Read/Write/Shell/WebFetch/WebSearch/Mcp はこの deny で実際にゲートされる（実測 #682:
//!    `readToolCall`→error、`shellToolCall`→permissionDenied）。deny 下の grok 系は native
//!    read が拒否されると注入済み XML `<function_calls>` へフォールバックし、正しい形で
//!    opencrab のツール選択を出す（既存 parser がそのまま処理）。
//! 2. **空の専用 cwd（#682）**: chat_completion 毎に空の一時ディレクトリ
//!    （[`tempfile::TempDir`]、RAII で削除・孤児を残さない）を作り CLI の cwd にする。
//!    役割は (a) 実 workspace を cwd として露出させない（相対パスの native 読取や
//!    codebase_search の索引対象を空にする）こと、(b) 上記 cli.json の置き場。かつての
//!    「per-agent workspace を cwd にする」方式は実 repo を丸ごと露出したため**廃止**した。
//!
//! さらに `--plan`（読取専用モード）で write/shell を、`--sandbox`（既定 enabled）を
//! 重ね、`--trust` で信頼確認プロンプトのハングを避ける（`--force`/`--yolo` は使わない
//! ＝ 危険操作を承認なしで走らせない）。
//!
//! **【塞げていない穴・#682 でオーナー裁定により受容】**: native の **grep（および glob）は
//! cursor-agent の node プロセス内で同梱 `rg` を直接実行**するため、cli.json の権限系
//! （Read/Shell 等）も `--sandbox` の管轄外にある。実測（#682）で `Grep(**)` を deny に
//! 入れても `grepToolCall` は success で実データを返し、プロンプトで**絶対パス**を与えれば
//! 空 cwd の外のファイル内容も読めた（空 cwd は grep/glob の絶対パス読取を塞がない）。
//! これを機構で塞げるのは OS レベル sandbox（`sandbox-exec`）だけだが、複雑さを避けて
//! **不採用**とし、任意パス読取のリスクは受容してモデルの判断に委ねる（#682 裁定）。
//! `Grep(**)` を cli.json に列挙しないのは、効かないものを列挙して「効く」と誤認させない
//! ため。この穴を「空 cwd が塞ぐ」等と書いてはならない（実測で否定済み）。
//!
//! **協定不成立はエラーにしない**（#682）: モデルが XML を出さずテキストで答えたら
//! そのまま最終発話として返す（隠れ native フォールバックもエラー化もしない）。
//!
//! プロンプトは **positional 引数**で渡す。cursor-agent の headless（`-p`）は positional
//! を主インターフェースにしており、positional 無し（stdin 待ち）だと入力終端を待って
//! ハングする既知の不具合がある（公式フォーラム報告）。codex は `-` で stdin を明示
//! 指定できるが cursor-agent にその契約は無いため、確実な positional を採る。
//!
//! 子プロセスの環境変数は最小化する（[`minimal_env`]）。親 env（他プロバイダの
//! トークン類）を継承させず、`PATH` / `HOME` と、config 指定時のみ `CURSOR_API_KEY`
//! だけを渡す。
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
/// 既定モデル。account に依存しない `auto`（Cursor が適切なモデルを選ぶ）を採る。
/// かつて既定だった `gpt-5` は現行 CLI では無効（`Cannot use this model`）。
const DEFAULT_MODEL: &str = "auto";
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// `--sandbox` の既定値。最安全側（enabled）。config の `sandbox` で上書き可。
const DEFAULT_SANDBOX: &str = "enabled";

/// 一時 cwd に置く `.cursor/cli.json`（プロジェクト単位の permission 設定）の中身。
/// deny が allow に優先する。deny 下の grok 系を native read 拒否 → 注入済み XML
/// `<function_calls>` フォールバックへ誘導する駆動源であり、write/shell も封じる。
/// Read/Write/Shell/WebFetch/WebSearch/Mcp はこの deny で実際にゲートされる（実測 #682）。
///
/// - `version` キーは付けない（project 版は schema エラーで弾かれる。実測 #682）
/// - **grep / glob は deny が効かないので列挙しない**。効かないものを列挙して「効く」と
///   誤認させないため（実測 #682: `Grep(**)` を入れても `grepToolCall` は success で実データ
///   を返す。grep は node プロセス内の同梱 `rg` 直呼びで cli.json の管轄外）。この穴は
///   機構では塞げず、オーナー裁定で受容している（モジュール doc 参照）。
const CURSOR_DENY_CONFIG: &str = r#"{"permissions":{"allow":[],"deny":["Read(**)","Write(**)","Shell(**)","WebFetch(**)","WebSearch(**)","Mcp(**)"]}}"#;

/// ダッシュボード表示用の既定モデル候補（ID, context_window）。
/// 実際に選べるモデルは account・CLI バージョンで変わる（`cursor-agent models` /
/// `--list-models` で確認）ため、config の `models` で上書きするのが正確。ここは
/// あくまで初期候補で、無効モデルを既定に置かないための現行有効 ID を並べる。
static DEFAULT_MODELS: &[(&str, u32)] = &[
    ("auto", 200_000),
    ("gpt-5.2", 400_000),
    ("claude-opus-5", 200_000),
    ("claude-sonnet-5", 200_000),
];

#[derive(Debug, Clone)]
pub struct CursorProvider {
    binary_path: String,
    default_model: String,
    timeout: Duration,
    extra_models: Vec<(String, u32)>,
    /// `--sandbox` の値（"enabled" | "disabled"）。既定は最安全側 "enabled"。
    /// 読取専用モード（`--plan`）と直交する多層防御。
    sandbox: String,
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
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            extra_models: Vec::new(),
            sandbox: DEFAULT_SANDBOX.to_string(),
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

    /// `--sandbox` の値を上書きする（"enabled" | "disabled"）。空文字は既定
    /// （"enabled"）を維持する。読取専用モードは別途 `--plan` で常時有効なので、
    /// これはあくまで多層防御のサンドボックス層の切り替え。
    pub fn with_sandbox(mut self, sandbox: impl Into<String>) -> Self {
        let s = sandbox.into();
        if !s.trim().is_empty() {
            self.sandbox = s;
        }
        self
    }

    /// `CURSOR_API_KEY` を設定する。空文字は「未設定」として扱う。
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        let k = key.into();
        self.api_key = if k.trim().is_empty() { None } else { Some(k) };
        self
    }

    /// コマンドと、その cwd に使う空の一時ディレクトリを組み立てる。
    ///
    /// chat_completion 毎に空の [`tempfile::TempDir`] を作り、その中に deny 設定
    /// （[`CURSOR_DENY_CONFIG`]）だけを置いて CLI の cwd にする。返した `TempDir` は
    /// **呼び出し側が子プロセス完了まで保持**する必要がある（drop でディレクトリごと
    /// 削除され、孤児を残さない）。
    fn build_command(&self, model: &str, prompt: &str) -> Result<(Command, tempfile::TempDir)> {
        // 空の専用 cwd を作り、deny 設定だけを置く。実 workspace を cwd として露出させない
        // （相対パスの native 読取や codebase_search の索引対象を空にする）ためで、cli.json
        // の置き場も兼ねる。※ grep/glob は絶対パスを与えれば cwd の外を読めるので、空 cwd は
        // それを塞がない（#682・受容済み。CURSOR_DENY_CONFIG とモジュール doc 参照）。
        let cwd = tempfile::TempDir::new().context("failed to create temp cwd for cursor-agent")?;
        write_deny_config(cwd.path())?;

        let mut cmd = Command::new(resolve_binary(&self.binary_path));
        // タイムアウト/ドロップ時に子プロセスを確実に kill（孤児 agent を残さない）。
        cmd.kill_on_drop(true);
        cmd.arg("-p") // print / headless
            .arg("--output-format")
            .arg("json")
            // モデルは長形式 `--model`。この CLI 版は短縮 `-m` を受け付けない
            // （`unknown option '-m'`。実測 #674）。
            .arg("--model")
            .arg(model)
            // 推論専用（#674）: 読取専用モードで cursor-agent 自身の write/shell を封じる。
            // `--force`/`--yolo` は使わない（危険操作を承認なしで走らせない）。
            .arg("--plan")
            // サンドボックスを明示（既定 enabled = 最安全）。--plan と直交する多層防御。
            .arg("--sandbox")
            .arg(&self.sandbox)
            // 信頼確認プロンプトでハングしないよう workspace を信頼する。--force を外した
            // ので必須（無いと "Workspace Trust Required" で応答が返らない）。--trust は
            // ディレクトリ信頼のみで、write/shell 許可は与えない（それは --plan が封じる）。
            .arg("--trust")
            // プロンプトは positional で渡す（stdin 待ちハング回避）。OpenCrab の
            // プロンプトは常に `[Available Tools]`/`[System]` 等で始まりオプション
            // （`-` 始まり）と衝突しない。
            .arg(prompt);

        // 子プロセスの env を最小化する。親 env（他プロバイダのトークン類）を継承させず、
        // 必要分（PATH / HOME、config 指定時のみ CURSOR_API_KEY）だけを明示的に渡す。
        cmd.env_clear();
        for (key, value) in minimal_env(self.api_key.as_deref()) {
            cmd.env(key, value);
        }

        // cwd は空の専用一時ディレクトリ（実 workspace を露出させない・cli.json 置き場）。
        // grep/glob の絶対パス読取は塞げない点は上記参照（#682・受容済み）。
        cmd.current_dir(cwd.path());

        Ok((cmd, cwd))
    }
}

/// 一時 cwd に `.cursor/cli.json`（deny 設定）を書き出す。cursor-agent はプロジェクト
/// 単位の permission をこのパスから読む。
fn write_deny_config(cwd: &std::path::Path) -> Result<()> {
    let dir = cwd.join(".cursor");
    std::fs::create_dir_all(&dir).context("failed to create .cursor dir for cursor-agent")?;
    std::fs::write(dir.join("cli.json"), CURSOR_DENY_CONFIG)
        .context("failed to write .cursor/cli.json for cursor-agent")?;
    Ok(())
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

/// cursor-agent 子プロセスに渡す最小 env を組み立てる。`env_clear` で親 env を捨てた
/// 上でこれだけを渡し、他プロバイダのトークン類を継承させない（#674）。
///
/// - `PATH`: バイナリ / node ランタイムの解決に必須
/// - `HOME`: cursor-agent の launcher スクリプトが `$HOME` を参照し、無いと即死する
///   （`HOME: unbound variable`）。アンビエント認証の資格情報も HOME 配下にある
/// - `CURSOR_API_KEY`: config で api_key を指定したときだけ明示的に渡す。未指定なら
///   渡さず `cursor-agent login` 済みのアンビエント認証（HOME 配下）に任せる
///
/// 親に `PATH` / `HOME` が無い異常環境では該当エントリを落とす（存在するものだけ渡す）。
fn minimal_env(api_key: Option<&str>) -> Vec<(&'static str, String)> {
    let mut env: Vec<(&'static str, String)> = Vec::new();
    if let Ok(path) = std::env::var("PATH") {
        env.push(("PATH", path));
    }
    if let Ok(home) = std::env::var("HOME") {
        env.push(("HOME", home));
    }
    if let Some(key) = api_key {
        env.push(("CURSOR_API_KEY", key.to_string()));
    }
    env
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

    // #676: cursor-agent CLI 経由で max_tokens を送る口が無いため出力上限の登録は不要（opt-out）。
    fn sends_max_output_tokens(&self) -> bool {
        false
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

        // `_cwd` は子プロセス完了まで保持する（drop で一時 cwd ごと削除される。#682）。
        let (mut cmd, _cwd) = self.build_command(model, &prompt)?;
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

        // usage（camelCase）を JSON から拾う。取れなければ全ゼロ。
        let usage = parse_cursor_usage(&String::from_utf8_lossy(&output.stdout));

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
            usage,
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

    match find_result_object(&stdout_s).and_then(|v| extract_result(&v)) {
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

/// stdout から `"result"` を持つ JSON オブジェクトを拾って返す。
/// 出力全体（単一 JSON オブジェクト）を優先し、ダメなら末尾行から順に試す
/// （stream-json 混在や前後ノイズに耐える）。result / is_error / usage は
/// この 1 つのオブジェクトから取り出す。
fn find_result_object(stdout: &str) -> Option<serde_json::Value> {
    let whole = stdout.trim();
    let candidates = std::iter::once(whole).chain(stdout.lines().rev());
    for candidate in candidates {
        let c = candidate.trim();
        if !c.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(c) {
            if v.get("result").and_then(|r| r.as_str()).is_some() {
                return Some(v);
            }
        }
    }
    None
}

/// 結果オブジェクトから (result, is_error) を取り出す。
fn extract_result(v: &serde_json::Value) -> Option<(String, bool)> {
    let result = v.get("result").and_then(|r| r.as_str())?;
    let is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
    Some((result.to_string(), is_error))
}

/// cursor-agent の `--output-format json` は `usage` を camelCase で返す
/// （`inputTokens` / `outputTokens` / `cacheReadTokens` / `cacheWriteTokens`）。
/// codex（snake_case）とは別形式なので専用にパースする。見つからなければ全ゼロ。
fn parse_cursor_usage(stdout: &str) -> Usage {
    let field = |v: &serde_json::Value, key: &str| -> u32 {
        v.get(key).and_then(|n| n.as_u64()).unwrap_or(0) as u32
    };
    if let Some(usage) = find_result_object(stdout).and_then(|v| v.get("usage").cloned()) {
        let input = field(&usage, "inputTokens");
        let output = field(&usage, "outputTokens");
        return Usage {
            prompt_tokens: input,
            completion_tokens: output,
            total_tokens: input + output,
            cache_read_input_tokens: field(&usage, "cacheReadTokens"),
            cache_creation_input_tokens: field(&usage, "cacheWriteTokens"),
        };
    }
    Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    }
}

#[cfg(test)]
mod tests;
