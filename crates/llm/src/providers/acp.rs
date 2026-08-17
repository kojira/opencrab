//! ACP（Agent Client Protocol）エージェントを LLM プロバイダとして扱う。
//!
//! codex/cursor と同じ「別コマンドを spawn して制御し、最終テキストを受け取る」ワンショット
//! 型プロバイダ。違いは、単発 CLI 実行ではなく **改行区切り JSON-RPC 2.0 over stdio** で
//! ACP エージェント（`gemini --experimental-acp` / `npx @zed-industries/claude-code-acp` 等）を
//! 駆動する点。ACP エージェントは**自前でエージェントループ（ツール実行含む）**を回すため、
//! OpenCrab は 1 プロンプトを送って最終メッセージを集めるだけでよい（codex 同様）。
//!
//! シーケンス（no-auth）:
//!   initialize → session/new → session/prompt → (session/update で本文を蓄積) → 結果(stopReason)。
//! ツール定義はネイティブ function calling が無いのでプロンプトへ XML で載せる（codex 共通の
//! [`build_cli_prompt`]）。起動コマンド/引数はエージェントによって異なるため `binary_path`/`args`
//! で設定する。認証はエージェント側の env/ログインに委ねる（`authMethods` があれば best-effort）。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::codex::build_cli_prompt;
use crate::message::*;
use crate::traits::{LlmProvider, ModelInfo};

/// 既定バイナリ名（環境に応じて `binary_path`/`args` で変更する。単独では ACP モードに
/// ならないエージェントが多いので、実運用では args を設定する想定）。
const DEFAULT_ACP_PATH: &str = "acp-agent";
/// ACP は session/new にモデル指定が無く、モデルはエージェント自身の設定に従う。ここでの
/// model は UI/ルーティング（`acp:<model>`）用の名目値。
const DEFAULT_MODEL: &str = "default";
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// 失敗時に添える stderr 末尾の保持行数（起動失敗・認証エラー等の切り分け用）。
const STDERR_TAIL_LINES: usize = 30;
/// このクライアントが名乗る ACP プロトコルバージョン（**整数**。文字列ではない）。
const ACP_PROTOCOL_VERSION: u64 = 1;

/// 名目モデル（UI のドロップダウン用）。ACP は実モデルをエージェント設定で決める。
const DEFAULT_MODELS: &[(&str, u32)] = &[("default", 200_000)];

pub struct AcpProvider {
    binary_path: String,
    args: Vec<String>,
    default_model: String,
    working_dir: Option<String>,
    timeout: Duration,
    extra_models: Vec<(String, u32)>,
    /// テレメトリ用の表示名（既定は形式名 "acp"）。ルーティングキーは
    /// router 登録時に別途決まる。
    name: String,
}

impl Default for AcpProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpProvider {
    pub fn new() -> Self {
        Self {
            binary_path: DEFAULT_ACP_PATH.to_string(),
            args: Vec::new(),
            default_model: DEFAULT_MODEL.to_string(),
            working_dir: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            extra_models: Vec::new(),
            name: "acp".to_string(),
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

    /// ACP モードにするための起動引数（例 `["--experimental-acp"]`、
    /// `["-y","@zed-industries/claude-code-acp"]`）。
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
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
        let d = dir.into();
        if !d.trim().is_empty() {
            self.working_dir = Some(d);
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

    /// session/new に渡す **絶対** cwd（per-agent workspace）。子プロセスの cwd にも使う。
    fn workspace_cwd(&self, agent_id: Option<&str>) -> String {
        let dir = agent_id
            .map(|id| format!("data/agents/{id}/workspace"))
            .or_else(|| self.working_dir.clone())
            .unwrap_or_else(|| ".".to_string());
        std::fs::create_dir_all(&dir).ok();
        std::fs::canonicalize(&dir)
            .unwrap_or_else(|_| std::path::PathBuf::from(&dir))
            .to_string_lossy()
            .to_string()
    }
}

#[async_trait]
impl LlmProvider for AcpProvider {
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
            if !models.iter().any(|m| &m.id == id) {
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
        let model = if request.model.trim().is_empty() {
            self.default_model.clone()
        } else {
            request.model.clone()
        };
        let prompt = build_cli_prompt(&request);
        let cwd = self.workspace_cwd(request.agent_id.as_deref());

        let mut cmd = Command::new(resolve_binary(&self.binary_path));
        cmd.args(&self.args);
        cmd.kill_on_drop(true);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // stderr は継続的に drain して末尾を保持する（読まずに放置すると buffer 詰まりで
            // エージェントが止まりうるため必ず drain する）。起動失敗・認証エラーの切り分けに使う。
            .stderr(std::process::Stdio::piped());
        cmd.current_dir(&cwd);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("ACP エージェント起動に失敗: {}", self.binary_path))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("ACP: stdin ハンドルが取れません"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("ACP: stdout ハンドルが取れません"))?;
        // stderr 末尾を保持する drain タスクを起こす（detached。child kill で EOF→終了）。
        let stderr_tail = child.stderr.take().map(spawn_stderr_tail);

        // セッション全体を timeout でくくる。timeout/失敗時は child が drop され
        // kill_on_drop で kill される。
        let driven = tokio::time::timeout(
            self.timeout,
            drive_acp_session(Box::new(stdin), stdout, prompt, cwd),
        )
        .await;
        // 失敗時は stderr の末尾を添えて可視化する。
        let suffix = || stderr_tail.as_ref().map(stderr_suffix).unwrap_or_default();
        let (text, stop_reason) = match driven {
            Err(_) => {
                return Err(anyhow!(
                    "ACP エージェントが {}s でタイムアウトしました{}",
                    self.timeout.as_secs(),
                    suffix()
                ))
            }
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(anyhow!("{e}{}", suffix())),
        };

        let content = if text.trim().is_empty() {
            None
        } else {
            Some(MessageContent::Text(text))
        };

        Ok(ChatResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model,
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
                finish_reason: Some(finish_reason_from_stop(&stop_reason)),
            }],
            // ACP には usage_update があるが v1 では追わない（0）。
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
        // 実際に ACP エージェントを起動し initialize ハンドシェイクが通るかを確認する。
        // `<binary> --version` は npx ラッパ（binary=npx, args=[-y, @…/claude-code-acp]）等で
        // 常に成功し「ACP を起こせるか」を反映しない。#118 の自動ロールバックや接続テストが
        // 壊れた設定を誤って成功判定しないよう、本物のハンドシェイクで確認する。
        let cwd = self.workspace_cwd(None);
        let mut cmd = Command::new(resolve_binary(&self.binary_path));
        cmd.args(&self.args);
        cmd.kill_on_drop(true);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        cmd.current_dir(&cwd);
        let Ok(mut child) = cmd.spawn() else {
            return Ok(false);
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Ok(false);
        };
        // 起動確認は短時間で（長い chat timeout をそのまま使わず 15s を上限に）。
        let probe_timeout = self.timeout.min(Duration::from_secs(15));
        let ok = tokio::time::timeout(
            probe_timeout,
            acp_initialize_handshake(Box::new(stdin), stdout),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);
        // child は drop 時に kill_on_drop で終了する。
        drop(child);
        Ok(ok)
    }
}

/// 子プロセスの stderr を継続的に読み、末尾 [`STDERR_TAIL_LINES`] 行を保持する
/// detached タスクを起こす。stderr を読まず放置すると pipe buffer 詰まりで
/// エージェントが止まりうるため、パイプするなら必ず drain する。child が kill されると
/// EOF に達しタスクは自然終了する。
fn spawn_stderr_tail(stderr: tokio::process::ChildStderr) -> Arc<Mutex<VecDeque<String>>> {
    let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let tail_w = tail.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut t = tail_w.lock().unwrap();
            if t.len() >= STDERR_TAIL_LINES {
                t.pop_front();
            }
            t.push_back(line);
        }
    });
    tail
}

/// 保持した stderr 末尾をエラーに添える文字列にする（空なら空文字）。
fn stderr_suffix(tail: &Arc<Mutex<VecDeque<String>>>) -> String {
    let t = tail.lock().unwrap();
    if t.is_empty() {
        String::new()
    } else {
        format!(
            "\n--- ACP エージェントの stderr（末尾 {} 行）---\n{}",
            t.len(),
            t.iter().cloned().collect::<Vec<_>>().join("\n")
        )
    }
}

/// トランスポート上で `initialize` ハンドシェイクだけを行い、相手が ACP を話せる
/// （＝実際に起動できた）かを確認する。`health_check` の実体で、in-memory パイプで
/// テストできるよう transport を抽象化している。
///
/// 注意: 本関数は**内部 timeout を持たない**（相手が無反応なら `rx.await` で待ち続ける）。
/// 呼び出し側は必ず `tokio::time::timeout` でくくること（`health_check` はそうしている）。
async fn acp_initialize_handshake<R>(
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    reader: R,
) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
{
    let writer = Arc::new(tokio::sync::Mutex::new(writer));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let text = Arc::new(Mutex::new(String::new()));
    let reader_task = spawn_reader(reader, writer.clone(), pending.clone(), text);
    struct Abort(JoinHandle<()>);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _abort = Abort(reader_task);

    let id = 1u64;
    let (tx, rx) = oneshot::channel();
    pending.lock().unwrap().insert(id, tx);
    let msg = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false
            },
            "clientInfo": {"name": "opencrab", "version": env!("CARGO_PKG_VERSION")}
        }
    });
    if let Err(e) = write_line(&writer, &msg).await {
        pending.lock().unwrap().remove(&id);
        return Err(anyhow!("ACP initialize 送信失敗: {e}"));
    }
    match rx.await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(anyhow!("ACP initialize エラー: {e}")),
        Err(_) => Err(anyhow!("ACP: initialize 前に接続が閉じました")),
    }
}

/// `binary_path` を spawn 用に解決する（cursor と同じ。cwd を per-agent workspace に切り替える
/// ため、ディレクトリ付き相対パスはサーバ cwd 基準で絶対化する）。
fn resolve_binary(path: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if path.contains('/') && p.is_relative() {
        std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
    } else {
        p.to_path_buf()
    }
}

/// ACP の stopReason を OpenCrab の FinishReason に写す。
fn finish_reason_from_stop(stop: &str) -> FinishReason {
    match stop {
        "max_tokens" => FinishReason::Length,
        "refusal" => FinishReason::ContentFilter,
        // end_turn / max_turn_requests / cancelled / 未知 → 通常終了扱い。
        _ => FinishReason::Stop,
    }
}

/// session/request_permission の options から自動選択する optionId を選ぶ。
/// ACP エージェント（バックエンド）は運用者が選んで登録したものなので、既定は
/// **許可**（allow_once → allow_always）してループを完走させる。許可肢が無ければ
/// reject（reject_once → reject_always）を選び、無ければ先頭。
fn pick_permission_option(options: &Value) -> Option<String> {
    let arr = options.as_array()?;
    let by_kind = |kind: &str| {
        arr.iter()
            .find(|o| o.get("kind").and_then(|k| k.as_str()) == Some(kind))
            .and_then(|o| o.get("optionId").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
    };
    by_kind("allow_once")
        .or_else(|| by_kind("allow_always"))
        .or_else(|| by_kind("reject_once"))
        .or_else(|| by_kind("reject_always"))
        .or_else(|| {
            arr.first()
                .and_then(|o| o.get("optionId").and_then(|v| v.as_str()))
                .map(|s| s.to_string())
        })
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>>>;

/// ACP セッションを1回駆動して (最終本文, stopReason) を返す。トランスポート抽象化で
/// in-memory パイプでもテストできる。
async fn drive_acp_session<R>(
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    reader: R,
    prompt: String,
    cwd: String,
) -> Result<(String, String)>
where
    R: AsyncRead + Send + Unpin + 'static,
{
    let writer = Arc::new(tokio::sync::Mutex::new(writer));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let text = Arc::new(Mutex::new(String::new()));
    let next_id = AtomicU64::new(1);

    let reader_task = spawn_reader(reader, writer.clone(), pending.clone(), text.clone());
    // reader_task を確実に畳むためのガード（早期 return でも abort する）。
    struct Abort(JoinHandle<()>);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _abort = Abort(reader_task);

    let request = |method: &str, params: Value| {
        let writer = writer.clone();
        let pending = pending.clone();
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let method = method.to_string();
        async move {
            let (tx, rx) = oneshot::channel();
            pending.lock().unwrap().insert(id, tx);
            let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
            if let Err(e) = write_line(&writer, &msg).await {
                pending.lock().unwrap().remove(&id);
                return Err(anyhow!("ACP {method} 送信失敗: {e}"));
            }
            match rx.await {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(e)) => Err(anyhow!("ACP {method} エラー: {e}")),
                Err(_) => Err(anyhow!("ACP {method}: 接続が閉じました")),
            }
        }
    };

    // 1) initialize（fs/terminal 能力は出さない＝エージェントは fs/terminal を呼ばない）。
    let init = request(
        "initialize",
        json!({
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false
            },
            "clientInfo": {"name": "opencrab", "version": env!("CARGO_PKG_VERSION")}
        }),
    )
    .await?;
    if let Some(v) = init.get("protocolVersion").and_then(|v| v.as_u64()) {
        if v != ACP_PROTOCOL_VERSION {
            warn!(
                agent_version = v,
                client_version = ACP_PROTOCOL_VERSION,
                "ACP: プロトコルバージョン不一致（続行を試みます）"
            );
        }
    }

    // 認証が必要なら best-effort（methodId のみ。認証情報はエージェント側 env に委ねる）。
    if let Some(methods) = init.get("authMethods").and_then(|v| v.as_array()) {
        if !methods.is_empty() {
            let method_id = methods
                .first()
                .and_then(|m| m.get("id").and_then(|v| v.as_str()))
                .unwrap_or_default()
                .to_string();
            request("authenticate", json!({"methodId": method_id}))
                .await
                .map_err(|e| {
                    anyhow!(
                        "ACP 認証に失敗: {e}（エージェント側でログイン/認証を済ませてください）"
                    )
                })?;
        }
    }

    // 2) session/new（cwd は絶対パス、mcpServers は空）。
    let new_session = request("session/new", json!({"cwd": cwd, "mcpServers": []})).await?;
    let session_id = new_session
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("ACP: session/new が sessionId を返しません"))?
        .to_string();

    // 3) session/prompt（結果はターン終了時に返る。本文は session/update で蓄積済み）。
    let prompt_res = request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": prompt}]
        }),
    )
    .await?;
    let stop_reason = prompt_res
        .get("stopReason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn")
        .to_string();

    let final_text = text.lock().unwrap().clone();
    Ok((final_text, stop_reason))
}

/// 1 行の JSON を書き出す（改行終端）。
async fn write_line(
    writer: &Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    msg: &Value,
) -> Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    let mut w = writer.lock().await;
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

/// stdout を読み、応答(id+result/error)・通知(method のみ)・受信リクエスト(method+id)を
/// メッセージ形状で振り分ける。受信リクエスト（session/request_permission 等）にはこの場で
/// 応答する（応答しないと session/prompt が返らない）。
fn spawn_reader<R>(
    reader: R,
    writer: Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    pending: Pending,
    text: Arc<Mutex<String>>,
) -> JoinHandle<()>
where
    R: AsyncRead + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
                continue; // JSON でない行（ログ等）はスキップ
            };
            let has_method = msg.get("method").is_some();
            // JSON-RPC 2.0 の id は**数値でも文字列でも**よい。分類は id の有無で行う
            // （数値決め打ちだと、文字列 id の受信リクエストを通知と誤分類して未応答→
            // ハングする）。自分が送る id は u64 なので、応答照合だけ as_u64 を使う。
            let has_id = msg.get("id").is_some();
            match (has_method, has_id) {
                // 受信リクエスト（agent → client）。応答が必須。id は生のまま echo する。
                (true, true) => {
                    let req_id = msg.get("id").cloned().unwrap_or(Value::Null);
                    handle_incoming_request(&writer, req_id, &msg).await;
                }
                // 自分のリクエストへの応答（自分の id は u64）。
                (false, true) => {
                    let resp_id = msg.get("id").and_then(|v| v.as_u64());
                    if let Some(tx) = resp_id.and_then(|id| pending.lock().unwrap().remove(&id)) {
                        if let Some(err) = msg.get("error") {
                            let m = err
                                .get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown error")
                                .to_string();
                            let _ = tx.send(Err(m));
                        } else {
                            let _ = tx.send(Ok(msg.get("result").cloned().unwrap_or(Value::Null)));
                        }
                    }
                }
                // 通知（session/update 等）。
                (true, false) => {
                    accumulate_update(&msg, &text);
                }
                _ => {}
            }
        }
        // 接続断: 未応答の待ち手は drop で解放される。
        pending.lock().unwrap().clear();
    })
}

/// session/update 通知から assistant 本文（agent_message_chunk のテキスト）を蓄積する。
fn accumulate_update(msg: &Value, text: &Arc<Mutex<String>>) {
    if msg.get("method").and_then(|v| v.as_str()) != Some("session/update") {
        return;
    }
    let update = match msg.get("params").and_then(|p| p.get("update")) {
        Some(u) => u,
        None => return,
    };
    if update.get("sessionUpdate").and_then(|v| v.as_str()) != Some("agent_message_chunk") {
        return; // thought/tool_call 等は無視
    }
    if update
        .get("content")
        .and_then(|c| c.get("type"))
        .and_then(|v| v.as_str())
        != Some("text")
    {
        return;
    }
    if let Some(chunk) = update
        .get("content")
        .and_then(|c| c.get("text"))
        .and_then(|v| v.as_str())
    {
        text.lock().unwrap().push_str(chunk);
    }
}

/// 受信リクエストに応答する。session/request_permission は自動選択で許可し、それ以外
/// （fs/terminal は能力を出していないので本来来ない）は method-not-found を返す。
async fn handle_incoming_request(
    writer: &Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    // JSON-RPC id は数値/文字列いずれもありうるので生の Value のまま echo する。
    req_id: Value,
    msg: &Value,
) {
    let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let response = if method == "session/request_permission" {
        let options = msg
            .get("params")
            .and_then(|p| p.get("options"))
            .cloned()
            .unwrap_or(Value::Null);
        match pick_permission_option(&options) {
            Some(opt) => {
                json!({"jsonrpc":"2.0","id":req_id,"result":{"outcome":{"outcome":"selected","optionId":opt}}})
            }
            None => {
                // 選べる肢が無い → キャンセル扱い（ツールを実行させない）。
                json!({"jsonrpc":"2.0","id":req_id,"result":{"outcome":{"outcome":"cancelled"}}})
            }
        }
    } else {
        debug!(
            method,
            "ACP: 未対応の受信リクエストに method-not-found を返します"
        );
        json!({"jsonrpc":"2.0","id":req_id,"error":{"code":-32601,"message":"method not supported by opencrab client"}})
    };
    let _ = write_line(writer, &response).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_finish_reason_from_stop() {
        assert!(matches!(
            finish_reason_from_stop("end_turn"),
            FinishReason::Stop
        ));
        assert!(matches!(
            finish_reason_from_stop("max_tokens"),
            FinishReason::Length
        ));
        assert!(matches!(
            finish_reason_from_stop("refusal"),
            FinishReason::ContentFilter
        ));
        assert!(matches!(
            finish_reason_from_stop("cancelled"),
            FinishReason::Stop
        ));
        assert!(matches!(
            finish_reason_from_stop("weird"),
            FinishReason::Stop
        ));
    }

    #[test]
    fn test_pick_permission_option() {
        let opts = json!([
            {"optionId":"r","name":"Reject","kind":"reject_once"},
            {"optionId":"a","name":"Allow","kind":"allow_once"}
        ]);
        // 許可肢を優先。
        assert_eq!(pick_permission_option(&opts), Some("a".to_string()));
        // 許可肢が無ければ reject。
        let only_reject = json!([{"optionId":"r","name":"Reject","kind":"reject_always"}]);
        assert_eq!(pick_permission_option(&only_reject), Some("r".to_string()));
        // 空/不正は None。
        assert_eq!(pick_permission_option(&json!([])), None);
        assert_eq!(pick_permission_option(&json!("x")), None);
    }

    #[test]
    fn test_accumulate_update() {
        let text = Arc::new(Mutex::new(String::new()));
        let mk = |t: &str| json!({"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text": t}}}});
        accumulate_update(&mk("Hello "), &text);
        accumulate_update(&mk("world"), &text);
        // thought は無視。
        accumulate_update(
            &json!({"method":"session/update","params":{"update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}}}),
            &text,
        );
        assert_eq!(*text.lock().unwrap(), "Hello world");
    }

    #[test]
    fn test_builder_and_name() {
        let p = AcpProvider::new()
            .with_binary_path("gemini")
            .with_args(vec!["--experimental-acp".to_string()])
            .with_default_model("gemini-2.5-pro")
            .with_timeout_secs(120);
        assert_eq!(p.name(), "acp");
        assert_eq!(p.binary_path, "gemini");
        assert_eq!(p.args, vec!["--experimental-acp"]);
        assert_eq!(p.default_model, "gemini-2.5-pro");
        assert_eq!(p.timeout, Duration::from_secs(120));
        assert!(!p.supports_function_calling());
        // 空指定は無視される。
        let p2 = AcpProvider::new().with_binary_path("").with_timeout_secs(0);
        assert_eq!(p2.binary_path, DEFAULT_ACP_PATH);
        assert_eq!(p2.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    }

    /// in-memory パイプでモック ACP エージェントを立て、initialize→new→prompt と、
    /// ターン中の session/request_permission 応答、agent_message_chunk 蓄積を検証する。
    #[tokio::test]
    async fn test_drive_session_over_duplex() {
        let (client_w, agent_r) = tokio::io::duplex(16384);
        let (agent_w, client_r) = tokio::io::duplex(16384);

        // モックエージェント。
        tokio::spawn(async move {
            let mut lines = BufReader::new(agent_r).lines();
            let mut w = agent_w;
            let mut permission_answered = false;
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
                let id = msg.get("id").cloned();
                // これは client → agent の応答（permission への返答）。
                if method.is_empty() && msg.get("result").is_some() {
                    permission_answered = true;
                    continue;
                }
                let result = match method {
                    "initialize" => {
                        Some(json!({"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}))
                    }
                    "session/new" => Some(json!({"sessionId":"sess-1"})),
                    "session/prompt" => {
                        // まず permission を要求 → 本文チャンク → 応答（end_turn）。
                        // 文字列 id で送る（JSON-RPC 2.0 は文字列 id を許す。数値決め打ちで
                        // 誤分類→未応答→ハングしないことの回帰テスト）。
                        let perm = json!({"jsonrpc":"2.0","id":"perm-1","method":"session/request_permission","params":{"sessionId":"sess-1","toolCall":{"toolCallId":"t1"},"options":[{"optionId":"ok","name":"Allow","kind":"allow_once"}]}});
                        let mut s = serde_json::to_string(&perm).unwrap();
                        s.push('\n');
                        let _ = w.write_all(s.as_bytes()).await;
                        let _ = w.flush().await;
                        // client の応答を待つ（同ループで permission_answered が立つ）。
                        for _ in 0..50 {
                            if permission_answered {
                                break;
                            }
                            if let Ok(Some(l)) = lines.next_line().await {
                                if let Ok(m) = serde_json::from_str::<Value>(&l) {
                                    if m.get("result").is_some() && m.get("method").is_none() {
                                        permission_answered = true;
                                    }
                                }
                            }
                        }
                        for chunk in ["Hi", " there"] {
                            let upd = json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":chunk}}}});
                            let mut s = serde_json::to_string(&upd).unwrap();
                            s.push('\n');
                            let _ = w.write_all(s.as_bytes()).await;
                            let _ = w.flush().await;
                        }
                        Some(json!({"stopReason":"end_turn"}))
                    }
                    _ => None,
                };
                if let (Some(id), Some(result)) = (id, result) {
                    let resp = json!({"jsonrpc":"2.0","id":id,"result":result});
                    let mut s = serde_json::to_string(&resp).unwrap();
                    s.push('\n');
                    let _ = w.write_all(s.as_bytes()).await;
                    let _ = w.flush().await;
                }
            }
        });

        let (text, stop) = drive_acp_session(
            Box::new(client_w),
            client_r,
            "hello".to_string(),
            "/tmp".to_string(),
        )
        .await
        .unwrap();
        assert_eq!(text, "Hi there");
        assert_eq!(stop, "end_turn");
    }

    #[tokio::test]
    async fn test_health_handshake_ok_when_initialize_answered() {
        let (client_w, agent_r) = tokio::io::duplex(8192);
        let (agent_w, client_r) = tokio::io::duplex(8192);
        // initialize に応答するモックエージェント。
        tokio::spawn(async move {
            let mut lines = BufReader::new(agent_r).lines();
            let mut w = agent_w;
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if msg.get("method").and_then(|v| v.as_str()) == Some("initialize") {
                    let id = msg.get("id").cloned().unwrap_or(json!(1));
                    let resp = json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"authMethods":[]}});
                    let mut s = serde_json::to_string(&resp).unwrap();
                    s.push('\n');
                    let _ = w.write_all(s.as_bytes()).await;
                    let _ = w.flush().await;
                }
            }
        });
        assert!(acp_initialize_handshake(Box::new(client_w), client_r)
            .await
            .is_ok());
    }

    #[test]
    fn test_stderr_suffix_empty_and_populated() {
        let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        assert_eq!(stderr_suffix(&tail), "");
        tail.lock().unwrap().push_back("boom".to_string());
        let s = stderr_suffix(&tail);
        assert!(s.contains("stderr"));
        assert!(s.contains("boom"));
    }

    #[tokio::test]
    async fn test_stderr_tail_captures_lines() {
        // 実サブプロセスの stderr を drain して末尾に取り込めること。
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("echo err1 >&2; echo err2 >&2")
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let tail = spawn_stderr_tail(child.stderr.take().unwrap());
        let _ = child.wait().await;
        // drain タスクが読み切るのを待つ。
        for _ in 0..50 {
            if tail.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let s = stderr_suffix(&tail);
        assert!(s.contains("err1"), "stderr tail should contain err1: {s}");
        assert!(s.contains("err2"), "stderr tail should contain err2: {s}");
    }

    #[tokio::test]
    async fn test_health_handshake_fails_when_connection_closes() {
        let (client_w, agent_r) = tokio::io::duplex(8192);
        let (agent_w, client_r) = tokio::io::duplex(8192);
        // 応答せず即クローズ（`npx --version` は通るが ACP を話せないエージェントの模擬）。
        drop(agent_r);
        drop(agent_w);
        assert!(acp_initialize_handshake(Box::new(client_w), client_r)
            .await
            .is_err());
    }
}
