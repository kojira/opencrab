//! MCP stdio クライアント（改行区切り JSON-RPC 2.0）。
//!
//! 薄い自前実装（このリポジトリの流儀）: 外部 SDK に依存せず、`initialize` →
//! `notifications/initialized` → `tools/list` → `tools/call` だけを話す。相関は
//! request id ↔ oneshot で行い、読み取りは背後タスクで回す。トランスポートは
//! `AsyncWrite`(stdin) + `AsyncRead`(stdout) に抽象化してあり、テストは in-memory
//! パイプ（`tokio::io::duplex`）で行える。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::config::McpServerConfig;

/// このクライアントが名乗る MCP プロトコルバージョン（サーバが折衝する）。
const PROTOCOL_VERSION: &str = "2025-06-18";
const CLIENT_NAME: &str = "opencrab";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// MCP サーバが公開する1ツール。
#[derive(Debug, Clone)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    /// JSON Schema（LLM にそのまま渡す `parameters`）。
    pub input_schema: Value,
}

/// `tools/call` の結果（テキスト連結 + エラーフラグ）。
#[derive(Debug, Clone)]
pub struct McpToolResult {
    /// content ブロックのテキストを連結したもの。
    pub text: String,
    /// サーバが `isError: true` を返したか。
    pub is_error: bool,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>>>;

/// 1 本の MCP 接続（トランスポート非依存のコア）。
pub struct McpConnection {
    writer: tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    pending: Pending,
    next_id: AtomicU64,
    request_timeout: Duration,
    reader_task: JoinHandle<()>,
}

impl Drop for McpConnection {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

impl McpConnection {
    /// writer(stdin 相当) と reader(stdout 相当) から接続を作り、読み取りタスクを起こす。
    pub fn new<R>(writer: Box<dyn AsyncWrite + Send + Unpin>, reader: R) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_task = tokio::spawn(read_loop(reader, pending.clone()));
        Self {
            writer: tokio::sync::Mutex::new(writer),
            pending,
            next_id: AtomicU64::new(1),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            reader_task,
        }
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        if !timeout.is_zero() {
            self.request_timeout = timeout;
        }
        self
    }

    /// 接続がまだ生きているか。読み取りタスクは stdout の EOF/エラー（＝サーバ終了・
    /// クラッシュ）で終了するため、`reader_task` の完了＝接続断とみなす。
    pub fn is_alive(&self) -> bool {
        !self.reader_task.is_finished()
    }

    async fn write_message(&self, msg: &Value) -> Result<()> {
        let mut line = serde_json::to_string(msg)?;
        line.push('\n');
        let mut w = self.writer.lock().await;
        w.write_all(line.as_bytes()).await?;
        w.flush().await?;
        Ok(())
    }

    /// id 付きリクエストを送り応答を待つ。
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.write_message(&msg).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }

        match tokio::time::timeout(self.request_timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(err))) => Err(anyhow!("MCP {method} error: {err}")),
            Ok(Err(_canceled)) => Err(anyhow!("MCP {method}: 接続が閉じました")),
            Err(_timeout) => {
                self.pending.lock().unwrap().remove(&id);
                Err(anyhow!(
                    "MCP {method}: タイムアウト（{}s）",
                    self.request_timeout.as_secs()
                ))
            }
        }
    }

    /// 通知（id 無し・応答無し）を送る。
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.write_message(&msg).await
    }

    /// `initialize` → `notifications/initialized` の握手を行う。
    pub async fn initialize(&self) -> Result<()> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")},
        });
        self.request("initialize", params).await?;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    /// `tools/list` を取得する。
    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let result = self.request("tools/list", json!({})).await?;
        parse_tools(&result)
    }

    /// `tools/call` を実行する。
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<McpToolResult> {
        let params = json!({"name": name, "arguments": arguments});
        let result = self.request("tools/call", params).await?;
        Ok(parse_tool_result(&result))
    }
}

impl McpClient {
    /// 接続が生きているか（サーバがクラッシュ/終了していないか）。
    pub fn is_alive(&self) -> bool {
        self.conn.is_alive()
    }
}

/// 起動済み MCP サーバ（stdio subprocess）への接続。`child` を保持し kill_on_drop で
/// プロセスを道連れにする。接続時に握手し `tools/list` をキャッシュする。
pub struct McpClient {
    server_name: String,
    conn: McpConnection,
    tools: Vec<McpTool>,
    // 落とすとプロセスが kill される（kill_on_drop）。順序: conn より後に drop されるよう
    // 最後に置く（conn.drop が reader_task を abort → stdout が閉じる）。
    _child: tokio::process::Child,
}

impl McpClient {
    /// 設定から MCP サーバを spawn し、握手して tools をキャッシュする。
    pub async fn connect(config: &McpServerConfig) -> Result<Self> {
        Self::connect_with_timeout(config, DEFAULT_REQUEST_TIMEOUT).await
    }

    pub async fn connect_with_timeout(
        config: &McpServerConfig,
        request_timeout: Duration,
    ) -> Result<Self> {
        use std::process::Stdio;
        let mut cmd = tokio::process::Command::new(&config.command);
        cmd.args(&config.args);
        for (k, v) in &config.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr は読まないと buffer 詰まりでサーバが止まりうるので捨てる。
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("MCP サーバ起動に失敗: {}", config.command))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("MCP: stdin ハンドルが取れません"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("MCP: stdout ハンドルが取れません"))?;

        let conn =
            McpConnection::new(Box::new(stdin), stdout).with_request_timeout(request_timeout);
        conn.initialize()
            .await
            .with_context(|| format!("MCP initialize 失敗: {}", config.name))?;
        let tools = conn
            .list_tools()
            .await
            .with_context(|| format!("MCP tools/list 失敗: {}", config.name))?;

        Ok(Self {
            server_name: config.name.clone(),
            conn,
            tools,
            _child: child,
        })
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// キャッシュ済みのツール一覧。
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// ツールを呼ぶ。
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<McpToolResult> {
        self.conn.call_tool(name, arguments).await
    }
}

/// stdout を1行ずつ読み、応答（id 付き）を pending の oneshot へ配送する。
/// 通知・不正な行はスキップする。EOF/エラーで終了する（pending は drop で解放）。
async fn read_loop<R>(reader: R, pending: Pending)
where
    R: AsyncRead + Send + Unpin + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
                    continue; // JSON でない行（ログ等）はスキップ
                };
                // 本クライアントは id を u64 でしか送らないので u64 一致で十分。
                // 万一サーバが文字列 id 等を返すと待ち手には届かず timeout で解消する。
                let Some(id) = msg.get("id").and_then(|v| v.as_u64()) else {
                    continue; // 通知（id 無し）はスキップ
                };
                let Some(tx) = pending.lock().unwrap().remove(&id) else {
                    continue; // 対応する待ち手が無い
                };
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
            Ok(None) => break, // EOF
            Err(_) => break,   // read error
        }
    }
    // 接続断: 未応答の待ち手は drop（= oneshot canceled）で解放される。
    pending.lock().unwrap().clear();
}

#[derive(Deserialize)]
struct RawTool {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    input_schema: Option<Value>,
}

fn parse_tools(result: &Value) -> Result<Vec<McpTool>> {
    let arr = result
        .get("tools")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("tools/list: 'tools' 配列がありません"))?;
    let mut out = Vec::with_capacity(arr.len());
    for t in arr {
        // 不正な1エントリ（name 欠落等）で全滅させない。壊れたものは飛ばして続行する。
        let Ok(raw) = serde_json::from_value::<RawTool>(t.clone()) else {
            tracing::warn!("tools/list: 不正なツール定義をスキップしました");
            continue;
        };
        out.push(McpTool {
            name: raw.name,
            description: raw.description.unwrap_or_default(),
            // schema 無しは空の object スキーマにフォールバック（LLM 互換のため）。
            input_schema: raw
                .input_schema
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        });
    }
    Ok(out)
}

fn parse_tool_result(result: &Value) -> McpToolResult {
    let is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut text = String::new();
    if let Some(content) = result.get("content").and_then(|v| v.as_array()) {
        for block in content {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                }
                // text 以外（image/resource 等）は種別だけ示す（v1 はテキスト中心）。
                Some(other) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&format!("[{other} content]"));
                }
                None => {}
            }
        }
    }
    if text.is_empty() {
        // content が無い/空でも、生の result を落とさず文字列化しておく。
        text = result.to_string();
    }
    McpToolResult { text, is_error }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tools() {
        let v = json!({"tools": [
            {"name": "read_file", "description": "read", "inputSchema": {"type":"object","properties":{"path":{"type":"string"}}}},
            {"name": "noschema"},
            {"description": "no name → skip"}
        ]});
        let tools = parse_tools(&v).unwrap();
        // 不正な1件（name 欠落）はスキップし、残り2件を返す。
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(
            tools[0].input_schema["properties"]["path"]["type"],
            "string"
        );
        // schema/description 無しはフォールバック。
        assert_eq!(tools[1].name, "noschema");
        assert_eq!(tools[1].description, "");
        assert_eq!(tools[1].input_schema["type"], "object");
        // 'tools' が無ければエラー。
        assert!(parse_tools(&json!({})).is_err());
    }

    #[test]
    fn test_parse_tool_result() {
        let r = parse_tool_result(
            &json!({"content": [{"type":"text","text":"hello"},{"type":"text","text":"world"}]}),
        );
        assert_eq!(r.text, "hello\nworld");
        assert!(!r.is_error);
        let e = parse_tool_result(
            &json!({"isError": true, "content": [{"type":"text","text":"boom"}]}),
        );
        assert!(e.is_error);
        assert_eq!(e.text, "boom");
        // 非テキストは種別表示。
        let img = parse_tool_result(&json!({"content": [{"type":"image","data":"..."}]}));
        assert_eq!(img.text, "[image content]");
    }

    /// in-memory パイプでモックサーバを立て、initialize→list→call の往復を検証。
    #[tokio::test]
    async fn test_roundtrip_over_duplex() {
        // client_w -> server_r, server_w -> client_r
        let (client_w, server_r) = tokio::io::duplex(8192);
        let (server_w, client_r) = tokio::io::duplex(8192);

        // モック MCP サーバ: 行ごとに JSON-RPC を読み、method に応じて応答する。
        tokio::spawn(async move {
            let mut lines = BufReader::new(server_r).lines();
            let mut sw = server_w;
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
                let id = msg.get("id").cloned();
                let result = match method {
                    "initialize" => Some(json!({"protocolVersion":"2025-06-18","capabilities":{}})),
                    "tools/list" => Some(
                        json!({"tools":[{"name":"echo","description":"e","inputSchema":{"type":"object"}}]}),
                    ),
                    "tools/call" => {
                        let args = msg
                            .get("params")
                            .and_then(|p| p.get("arguments"))
                            .cloned()
                            .unwrap_or(json!({}));
                        let m = args.get("msg").and_then(|v| v.as_str()).unwrap_or("");
                        Some(json!({"content":[{"type":"text","text": format!("echo:{m}")}]}))
                    }
                    _ => None, // notifications/initialized 等は応答しない
                };
                if let (Some(id), Some(result)) = (id, result) {
                    let resp = json!({"jsonrpc":"2.0","id":id,"result":result});
                    let mut s = serde_json::to_string(&resp).unwrap();
                    s.push('\n');
                    use tokio::io::AsyncWriteExt;
                    let _ = sw.write_all(s.as_bytes()).await;
                    let _ = sw.flush().await;
                }
            }
        });

        let conn = McpConnection::new(Box::new(client_w), client_r);
        conn.initialize().await.unwrap();
        let tools = conn.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let res = conn.call_tool("echo", json!({"msg": "hi"})).await.unwrap();
        assert_eq!(res.text, "echo:hi");
        assert!(!res.is_error);
    }

    #[tokio::test]
    async fn test_is_alive_flips_on_disconnect() {
        let (client_w, _server_r) = tokio::io::duplex(1024);
        let (server_w, client_r) = tokio::io::duplex(1024);
        let conn = McpConnection::new(Box::new(client_w), client_r);
        // 相手（サーバ側 writer）が生きている間は alive。
        assert!(conn.is_alive());
        // サーバ側 writer を drop → client_r が EOF → read_loop 終了 → dead。
        drop(server_w);
        // read_loop がEOFを検知して終了するのを待つ。
        for _ in 0..50 {
            if !conn.is_alive() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!conn.is_alive(), "接続断で is_alive は false になる");
    }
}
