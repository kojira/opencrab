use async_trait::async_trait;
use opencrab_core::tool_result_log::READ_TOOL_RESULT_TOKEN_LIMIT;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult, SideEffect};

pub struct WsReadAction;

#[async_trait]
impl Action for WsReadAction {
    fn name(&self) -> &str {
        "ws_read"
    }

    fn description(&self) -> &str {
        "ワークスペース内のファイルを読み取る。全文が inline 上限を超えると結果は退避される\
         （#284/#294）ので、大きなファイルは start_line / line_count で行範囲を指定して読む。\
         grep が返す行番号をそのまま start_line に渡せる。範囲を指定すれば退避されずに読め、\
         has_more なら next_line を start_line に入れて続きを辿れる。1 行が長すぎると切られ\
         末尾に ` …⟨+M文字⟩` が付く。継続は行単位なので、切られた行の続き（行内の残り）は\
         再取得できない（標識がそれを示す）。1 行しか無いファイルは常に同じ先頭 512 文字＋標識が\
         返り has_more=false になる。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "読み取るファイルのパス（ワークスペースルートからの相対パス）"
                },
                "start_line": {
                    "type": "integer",
                    "description": "読み始める行（1 始まり。grep が返す行番号をそのまま渡せる）。省略時は先頭。has_more が true のとき next_line を渡すと続きを読める。",
                    "default": 1
                },
                "line_count": {
                    "type": "integer",
                    "description": format!("返す行数（省略時はトークン予算まで）。1 行は最大 {WS_READ_MAX_LINE_CHARS} 文字で切られ、切られた行の末尾には ` …⟨+M文字⟩`（M は切り捨てた文字数）が付く。")
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let path = match args["path"].as_str() {
            Some(p) => p.to_string(),
            None => return ActionResult::error("path is required"),
        };
        // start_line / line_count のどちらかが与えられたら行範囲読み。無ければ従来どおり全文
        // （後方互換）。旧 offset / limit（バイト）は未知キーとして無視され、全文読みへ落ちる。
        let start_line = args["start_line"].as_u64().unwrap_or(1).max(1) as usize;
        // 0 行要求は無意味。1 に丸めて、next_line が start_line から進まない暴走ページングを防ぐ。
        // #707: 既定 2,000 行（Claude Code の Read と同じ）。省略時に全文を返して退避する形を
        // やめ、範囲指定の有無に関わらず「1 ページ目」を返す。続きは has_more / next_line。
        let line_count = Some(
            args["line_count"]
                .as_u64()
                .map(|n| (n as usize).max(1))
                .unwrap_or(WS_READ_DEFAULT_LINES),
        );

        // 読み取り・計数・組み上げ・トークナイズを **すべて `spawn_blocking` の中** で行う
        // （マルチエージェントの async executor を同期 CPU で塞がない / #567）。返すのは有界な
        // 結果 JSON だけで、巨大な全文（実測 509MB）を executor 側へ持ち帰らない。
        let ws = ctx.workspace.clone();
        match tokio::task::spawn_blocking(move || {
            compute_ws_read(&ws, &path, start_line, line_count)
        })
        .await
        {
            Ok(Ok(v)) => ActionResult::success(v),
            Ok(Err(e)) => ActionResult::error(&e.to_string()),
            Err(e) => ActionResult::error(&format!("read task failed: {e}")),
        }
    }
}

/// `ws_read` の本体（同期・`spawn_blocking` 内で走る）。行範囲読みは単一 open の逐次読みで
/// ページを組み、有界な結果 JSON を返す。
///
/// **行範囲読みは全ファイル長ぶんの走査も全文読みもしない。** 規模（`total_bytes`）は
/// `metadata().len()`（O(1)）で得る。本文は [`Workspace::line_reader`] が 1 回 open した窓の
/// 中で `start_line` から 1 行ずつ、各行 [`WS_READ_MAX_LINE_CHARS`] 文字までを積み、標識込みの
/// 累計が [`RANGE_CONTENT_TOKEN_CEILING`] に達する直前で止める（返り値は必ず inline 上限未満＝
/// 再退避しない / #564 の自己ループを断つ）。二分探索もバイト境界補正も無い（#617）。範囲指定
/// なしの全文読みだけは従来どおり全体を読む（後方互換）。
///
/// [`Workspace::line_reader`]: opencrab_core::workspace::Workspace::line_reader
fn compute_ws_read(
    ws: &opencrab_core::workspace::Workspace,
    path: &str,
    start_line: usize,
    line_count: Option<usize>,
) -> anyhow::Result<serde_json::Value> {
    // 行範囲読み: 単一 open の逐次読みで `start_line` から 1 行ずつページに積む。各行は
    // WS_READ_MAX_LINE_CHARS 文字で切り（超過は ` …⟨+M文字⟩` の標識を付ける）、標識込みの累計
    // がトークン上限に達する直前で止める。二分探索もバイト境界補正も無い（#617）。
    //
    // 予算は**行トークンの走行合計**で判定する。累積 content を毎行トークナイズし直すと、ページ
    // 確定までに O(n²) のトークナイズが走る（149 行で ~740KB を tiktoken に通していた）。各 piece
    // を 1 度だけ数えて足すと O(n) になる。部分文字列を別々に数えた和は必ず全体の真値以上（分割で
    // トークンは減らない）で、`RANGE_CONTENT_TOKEN_CEILING` の −400 余白に収まるので、過小評価で
    // 再退避が起きることはない（[`tokens_reach_limit`] の窓和と同じ安全側 / #576）。改行結合ぶんも
    // 1 トークン上限として足す（`\n` は 1 トークンだが、跨ぐ結合で増えることはあっても減らない）。
    let (mut reader, total_bytes) = ws.line_reader(path, start_line, WS_READ_MAX_LINE_CHARS)?;
    let mut content = String::new();
    let mut used_tokens = 0usize;
    let mut first_line: Option<usize> = None;
    let mut next_line: Option<usize> = None;
    let mut lines_taken = 0usize;
    while let Some(line) = reader.next_line()? {
        // line_count が与えられていれば行数でも頭打ちにする（トークン予算とどちらか早い方）。
        if line_count.is_some_and(|lc| lines_taken >= lc) {
            next_line = Some(line.number); // まだ続きがある
            break;
        }
        let piece = if line.overflow_chars > 0 {
            format!("{} …⟨+{}文字⟩", line.text, line.overflow_chars)
        } else {
            line.text
        };
        // この行を足したときの上限側トークン見積り（改行結合ぶん +1）。
        let joiner = if content.is_empty() { 0 } else { 1 };
        let piece_tokens = opencrab_core::tokens::estimate_tokens(&piece) + joiner;
        // 予算判定。ページに 1 行も無いうちは無条件で 1 行返す（最低 1 行保証: 単独で予算超過の
        // 行でも 512 文字に切って返し next_line を前進させる。ここを空返しにすると next_line が
        // start_line から進まず、同じ行を読み続ける暴走ページングになる / #567 の趣旨を行版で保つ）。
        if !content.is_empty() && used_tokens + piece_tokens > RANGE_CONTENT_TOKEN_CEILING {
            next_line = Some(line.number); // この行は含めず、次回ここから読み直す
            break;
        }
        if content.is_empty() {
            content = piece;
        } else {
            content.push('\n');
            content.push_str(&piece);
        }
        used_tokens += piece_tokens;
        first_line.get_or_insert(line.number);
        lines_taken += 1;
    }

    // 出力の estimated_tokens は確定したページ本文（有界）を 1 度だけ正確に数える（O(n) 1 回）。
    let estimated_tokens = opencrab_core::tokens::estimate_tokens(&content);
    let mut out = json!({
        "path": path,
        "content": content,
        "total_bytes": total_bytes,
        "start_line": first_line.unwrap_or(start_line),
        "has_more": next_line.is_some(),
        "estimated_tokens": estimated_tokens,
        "inline_limit_tokens": READ_TOOL_RESULT_TOKEN_LIMIT,
    });
    if let Some(n) = next_line {
        out["next_line"] = json!(n);
    }
    Ok(out)
}

/// 行範囲読みで返す本文のトークン上限。結果 JSON 全体（本文＋メタ情報の封筒）が inline 上限
/// [`TOOL_RESULT_TOKEN_LIMIT`] を超えて再退避される（#564 の自己ループ）ことを構造的に防ぐため、
/// 封筒ぶんの余白を引いた保守値にする。封筒は keys＋数値＋`path`＋標識で実測数百トークン以内
/// なので、余裕を持って 400 引く。
const RANGE_CONTENT_TOKEN_CEILING: usize = READ_TOOL_RESULT_TOKEN_LIMIT - 400;

/// 行範囲読みで 1 行あたりに返す最大文字数（`char` 数）。超える行は切って ` …⟨+M文字⟩` を付ける。
///
/// 単位は**文字数**。テキストを扱うので文字で切れば境界補正が要らない（`chars().take(n)` で済む）。
///
/// 値 2,000（#707）は **Claude Code の Read ツールと同じ基準**。同じ仕事をする道具の刻み方が
/// 桁で違う理由が無い（オーナー指摘）。旧値 512 は当時のページ天井 2,100 トークンに単一行を
/// 収めるための逆算値で、天井が上がったので外れる（2,000 × 4 = 8,000 < 29,600 なので、最悪
/// 密度でも単一行が天井に収まる性質は保つ＝最低 1 行保証）。
const WS_READ_MAX_LINE_CHARS: usize = 2_000;

/// `line_count` 省略時に返す行数（#707）。**Claude Code の Read と同じ 2,000 行**。
///
/// 以前は省略時に**全文**を返していた。全文が上限を超えると `sanitize_tool_result` が中身を
/// 別ファイルへ複製して退避するため、読むたびに `workspace/tmp` が増えていた（本番実測:
/// 4,255 ファイル・270MB）。**元がファイルなのだから複製に意味が無い**（オーナー指摘）。
/// 省略時も 1 ページ目として返せば退避は起きず、参照は**元のファイル名がそのまま**使える。
const WS_READ_DEFAULT_LINES: usize = 2_000;

pub struct WsWriteAction;

#[async_trait]
impl Action for WsWriteAction {
    fn name(&self) -> &str {
        "ws_write"
    }

    fn description(&self) -> &str {
        "ワークスペース内にファイルを書き込む"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "書き込むファイルのパス（ワークスペースルートからの相対パス）"
                },
                "content": {
                    "type": "string",
                    "description": "ファイルの内容"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let path = match args["path"].as_str() {
            Some(p) => p,
            None => return ActionResult::error("path is required"),
        };
        let content = match args["content"].as_str() {
            Some(c) => c,
            None => return ActionResult::error("content is required"),
        };

        match ctx.workspace.write(path, content).await {
            Ok(_) => ActionResult::success(json!({
                "path": path,
                "written": true,
            }))
            .with_side_effect(SideEffect::FileWritten {
                path: path.to_string(),
            }),
            Err(e) => ActionResult::error(&e.to_string()),
        }
    }
}

pub struct WsEditAction;

#[async_trait]
impl Action for WsEditAction {
    fn name(&self) -> &str {
        "ws_edit"
    }

    fn description(&self) -> &str {
        "ワークスペース内のファイルを差分編集する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path", "old_string", "new_string"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "編集するファイルのパス"
                },
                "old_string": {
                    "type": "string",
                    "description": "置換対象の文字列（ユニークである必要がある）"
                },
                "new_string": {
                    "type": "string",
                    "description": "置換後の文字列"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let path = match args["path"].as_str() {
            Some(p) => p,
            None => return ActionResult::error("path is required"),
        };
        let old = match args["old_string"].as_str() {
            Some(o) => o,
            None => return ActionResult::error("old_string is required"),
        };
        let new = match args["new_string"].as_str() {
            Some(n) => n,
            None => return ActionResult::error("new_string is required"),
        };

        match ctx.workspace.edit(path, old, new).await {
            Ok(_) => ActionResult::success(json!({
                "path": path,
                "edited": true,
            })),
            Err(e) => ActionResult::error(&e.to_string()),
        }
    }
}

pub struct WsListAction;

#[async_trait]
impl Action for WsListAction {
    fn name(&self) -> &str {
        "ws_list"
    }

    fn description(&self) -> &str {
        "ワークスペース内のディレクトリを一覧表示する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "一覧表示するディレクトリのパス（デフォルト: ルート）",
                    "default": ""
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let path = args["path"].as_str().unwrap_or("");

        match ctx.workspace.list(path).await {
            Ok(entries) => {
                let entries_json: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|e| {
                        json!({
                            "name": e.name,
                            "is_dir": e.is_dir,
                            "size": e.size,
                        })
                    })
                    .collect();
                ActionResult::success(json!({
                    "path": path,
                    "entries": entries_json,
                }))
            }
            Err(e) => ActionResult::error(&e.to_string()),
        }
    }
}

pub struct WsDeleteAction;

#[async_trait]
impl Action for WsDeleteAction {
    fn name(&self) -> &str {
        "ws_delete"
    }

    fn description(&self) -> &str {
        "ワークスペース内のファイルまたはディレクトリを削除する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "削除するファイルまたはディレクトリのパス"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let path = match args["path"].as_str() {
            Some(p) => p,
            None => return ActionResult::error("path is required"),
        };

        match ctx.workspace.delete(path).await {
            Ok(_) => ActionResult::success(json!({
                "path": path,
                "deleted": true,
            })),
            Err(e) => ActionResult::error(&e.to_string()),
        }
    }
}

pub struct WsMkdirAction;

#[async_trait]
impl Action for WsMkdirAction {
    fn name(&self) -> &str {
        "ws_mkdir"
    }

    fn description(&self) -> &str {
        "ワークスペース内にディレクトリを作成する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "作成するディレクトリのパス"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let path = match args["path"].as_str() {
            Some(p) => p,
            None => return ActionResult::error("path is required"),
        };

        match ctx.workspace.mkdir(path).await {
            Ok(_) => ActionResult::success(json!({
                "path": path,
                "created": true,
            })),
            Err(e) => ActionResult::error(&e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::*;
    use serde_json::json;

    fn test_context() -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: opencrab_db::Db::from_connection(conn),
            workspace: std::sync::Arc::new(ws),
            last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: std::sync::Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test-model".to_string(),
                active_model: None,
                available_providers: vec!["mock".to_string()],
                gateway: "test".to_string(),
            })),
            caller: CallerIdentity::Owner,
        };
        (dir, ctx)
    }

    #[tokio::test]
    async fn test_ws_write_and_read() {
        let (_dir, ctx) = test_context();
        let write_result = WsWriteAction
            .execute(&json!({"path": "test.txt", "content": "hello"}), &ctx)
            .await;
        assert!(write_result.success);

        let read_result = WsReadAction
            .execute(&json!({"path": "test.txt"}), &ctx)
            .await;
        assert!(read_result.success);
        let data = read_result.data.unwrap();
        assert_eq!(data["content"].as_str(), Some("hello"));
    }

    #[tokio::test]
    async fn test_ws_read_missing() {
        let (_dir, ctx) = test_context();
        let result = WsReadAction
            .execute(&json!({"path": "nonexistent.txt"}), &ctx)
            .await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_ws_list() {
        let (_dir, ctx) = test_context();
        WsWriteAction
            .execute(&json!({"path": "listed.txt", "content": "data"}), &ctx)
            .await;

        let result = WsListAction.execute(&json!({"path": ""}), &ctx).await;
        assert!(result.success);
        let data = result.data.unwrap();
        let entries = data["entries"].as_array().unwrap();
        let names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
        assert!(names.contains(&"listed.txt"));
    }

    #[tokio::test]
    async fn test_ws_edit() {
        let (_dir, ctx) = test_context();
        WsWriteAction
            .execute(&json!({"path": "edit.txt", "content": "old content"}), &ctx)
            .await;

        let edit_result = WsEditAction
            .execute(
                &json!({"path": "edit.txt", "old_string": "old", "new_string": "new"}),
                &ctx,
            )
            .await;
        assert!(edit_result.success);

        let read_result = WsReadAction
            .execute(&json!({"path": "edit.txt"}), &ctx)
            .await;
        assert!(read_result.success);
        let data = read_result.data.unwrap();
        assert_eq!(data["content"].as_str(), Some("new content"));
    }

    #[tokio::test]
    async fn test_ws_delete() {
        let (_dir, ctx) = test_context();
        WsWriteAction
            .execute(&json!({"path": "todelete.txt", "content": "bye"}), &ctx)
            .await;

        let del_result = WsDeleteAction
            .execute(&json!({"path": "todelete.txt"}), &ctx)
            .await;
        assert!(del_result.success);

        let read_result = WsReadAction
            .execute(&json!({"path": "todelete.txt"}), &ctx)
            .await;
        assert!(!read_result.success);
    }

    #[tokio::test]
    async fn test_ws_mkdir() {
        let (_dir, ctx) = test_context();
        let mkdir_result = WsMkdirAction
            .execute(&json!({"path": "newdir"}), &ctx)
            .await;
        assert!(mkdir_result.success);

        let list_result = WsListAction.execute(&json!({"path": ""}), &ctx).await;
        assert!(list_result.success);
        let data = list_result.data.unwrap();
        let entries = data["entries"].as_array().unwrap();
        let names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
        assert!(names.contains(&"newdir"));
    }

    /// 範囲指定なしは従来どおり全文を返す（後方互換）。規模メタ情報（バイト）が付く。
    #[tokio::test]
    async fn test_ws_read_no_range_returns_full_content() {
        let (_dir, ctx) = test_context();
        WsWriteAction
            .execute(
                &json!({"path": "f.txt", "content": "line1\nline2\nline3"}),
                &ctx,
            )
            .await;

        let r = WsReadAction.execute(&json!({"path": "f.txt"}), &ctx).await;
        assert!(r.success);
        let d = r.data.unwrap();
        assert_eq!(d["content"].as_str(), Some("line1\nline2\nline3"));
        assert_eq!(d["total_bytes"].as_u64(), Some(17));
        assert_eq!(d["has_more"].as_bool(), Some(false));
        // #617: バイト系フィールド（returned_bytes / next_offset）は廃止した。
        assert!(d.get("returned_bytes").is_none());
        assert!(d.get("next_line").is_none());
    }

    /// start_line / line_count（行）で行範囲だけを返し、next_line で続きを辿れる。grep の行番号を
    /// そのまま start_line に渡せる。
    #[tokio::test]
    async fn test_ws_read_line_range_returns_lines_and_paging_info() {
        let (_dir, ctx) = test_context();
        WsWriteAction
            .execute(
                &json!({"path": "f.txt", "content": "l1\nl2\nl3\nl4\nl5"}),
                &ctx,
            )
            .await;

        // 2 行目から 2 行。
        let r = WsReadAction
            .execute(
                &json!({"path": "f.txt", "start_line": 2, "line_count": 2}),
                &ctx,
            )
            .await;
        assert!(r.success);
        let d = r.data.unwrap();
        assert_eq!(d["content"].as_str(), Some("l2\nl3"));
        assert_eq!(d["start_line"].as_u64(), Some(2));
        assert_eq!(d["has_more"].as_bool(), Some(true));
        assert_eq!(d["next_line"].as_u64(), Some(4));

        // next_line から続きを読む。
        let r2 = WsReadAction
            .execute(
                &json!({"path": "f.txt", "start_line": 4, "line_count": 2}),
                &ctx,
            )
            .await;
        let d2 = r2.data.unwrap();
        assert_eq!(d2["content"].as_str(), Some("l4\nl5"));
        assert_eq!(d2["has_more"].as_bool(), Some(false), "末尾まで読んだ");
        assert!(d2.get("next_line").is_none());
    }

    /// start_line がファイル末尾を越えたら空・has_more=false（無限ページングにならない / テスト 1）。
    #[tokio::test]
    async fn test_ws_read_start_line_past_end_is_empty() {
        let (_dir, ctx) = test_context();
        WsWriteAction
            .execute(&json!({"path": "f.txt", "content": "a\nb\nc"}), &ctx)
            .await;

        let r = WsReadAction
            .execute(&json!({"path": "f.txt", "start_line": 100}), &ctx)
            .await;
        let d = r.data.unwrap();
        assert_eq!(d["content"].as_str(), Some(""));
        assert_eq!(d["has_more"].as_bool(), Some(false));
        assert!(d.get("next_line").is_none());
    }

    /// テスト 2: 単独で長い 1 行（改行の無い base64/ミニファイド）でも、行は 512 文字で切られ
    /// **最低 1 行**返る。切られた行には ` …⟨+M文字⟩` の標識が付く。続く行があれば next_line は
    /// 必ず start_line より前進する（暴走ページング防止 / #567 の趣旨を行版で保つ）。
    #[tokio::test]
    async fn test_ws_read_overlong_line_truncates_and_advances() {
        let (_dir, ctx) = test_context();
        // 1 行 5,000 文字（改行なし）を単独ファイルに。#707 で 1 行の上限が 2,000 文字に
        // なったので、「切られて標識が付く」ことを見るには素材もそれを超える必要がある。
        let huge = "a".repeat(5_000);
        WsWriteAction
            .execute(&json!({"path": "one.txt", "content": huge}), &ctx)
            .await;

        let r = WsReadAction
            .execute(&json!({"path": "one.txt", "start_line": 1}), &ctx)
            .await;
        assert!(r.success);
        let d = r.data.unwrap();
        let content = d["content"].as_str().unwrap();
        // 2,000 文字で切られ、標識が付く。切った文字数 M = 5,000 - 2,000 = 3,000。
        assert!(content.starts_with(&"a".repeat(2_000)));
        assert!(
            content.contains("…⟨+3000文字⟩"),
            "切った行に標識が付く: {content}"
        );
        // 単一行なので続きは無い。標識自体が「切られた」ことを伝える。
        assert_eq!(d["has_more"].as_bool(), Some(false));

        // 長い行のあとに別の行がある場合、その行は次ページへ回り next_line が前進する。
        let two = format!("{}\nsecond", "b".repeat(5_000));
        WsWriteAction
            .execute(&json!({"path": "two.txt", "content": two}), &ctx)
            .await;
        let r2 = WsReadAction
            .execute(
                &json!({"path": "two.txt", "start_line": 1, "line_count": 1}),
                &ctx,
            )
            .await;
        let d2 = r2.data.unwrap();
        assert!(d2["content"].as_str().unwrap().contains("…⟨+3000文字⟩"));
        assert_eq!(d2["has_more"].as_bool(), Some(true));
        assert_eq!(
            d2["next_line"].as_u64(),
            Some(2),
            "next_line は start_line(1) より前進する"
        );
    }

    /// テスト 7: 512 文字 1 行の**最悪密度**（4 バイト文字連続）でも、推定トークンは 2,048 以下に
    /// 収まる（o200k はバイト BPE で 4 バイト文字は最悪 4 トークンまで割れるが 512×4=2048<2100）。
    /// #707 の直接の検証: **2,000 行のファイルが 1 回で読める**。
    ///
    /// 修正前は 1 往復 2,000 トークン（上限 2,500）しか運べず、700 行の設計文書で 9 往復して
    /// も読み終わらなかった。1 往復ごとにモデルの推論（本番実測 100〜130 秒）が挟まるため、
    /// サブタスクが読解だけで 1,700 秒の制限に達し commit ゼロで終わった。
    ///
    /// 変異確認: 読みの上限を `TOOL_RESULT_TOKEN_LIMIT`（2,500）に戻すとこのテストは赤くなる。
    #[tokio::test]
    async fn test_ws_read_2000_lines_in_one_call() {
        let (_dir, ctx) = test_context();
        // 典型的なソース相当（1 行 40 文字 × 2,000 行 ≒ 20,000 トークン）。
        let line = "    let value = compute(argument);   // note";
        let src: String = std::iter::repeat_n(line, 2_000)
            .collect::<Vec<_>>()
            .join("\n");
        WsWriteAction
            .execute(&json!({"path": "src.rs", "content": src}), &ctx)
            .await;

        // 範囲を指定しない＝エージェントが普通に読む形。
        let r = WsReadAction.execute(&json!({"path": "src.rs"}), &ctx).await;
        assert!(r.success);
        let d = r.data.unwrap();
        assert_eq!(
            d["content"].as_str().unwrap().lines().count(),
            2_000,
            "2,000 行が 1 回で返らない（往復が増える＝#707 の状態）"
        );
        assert_eq!(
            d["has_more"].as_bool(),
            Some(false),
            "1 回で読み切れているなら続きは無い"
        );
        assert!(
            d["estimated_tokens"].as_u64().unwrap() <= READ_TOOL_RESULT_TOKEN_LIMIT as u64,
            "上限内＝退避されない（元がファイルなのに複製を作らない）: {}",
            d["estimated_tokens"]
        );
    }

    #[tokio::test]
    async fn test_ws_read_worst_density_line_under_token_ceiling() {
        let (_dir, ctx) = test_context();
        // U+20000（4 バイト）を 2,000 文字ちょうど＝最悪密度の 1 行（#707 で 1 行上限が
        // 2,000 文字になったので、その境界を突く）。overflow は出ない。
        let dense = "𠀀".repeat(2_000);
        WsWriteAction
            .execute(&json!({"path": "dense.txt", "content": dense}), &ctx)
            .await;

        let r = WsReadAction
            .execute(&json!({"path": "dense.txt", "start_line": 1}), &ctx)
            .await;
        let d = r.data.unwrap();
        assert_eq!(d["content"].as_str().unwrap().chars().count(), 2_000);
        assert!(
            d["estimated_tokens"].as_u64().unwrap() <= 8_192,
            "最悪密度でも 2,000 文字は 8,192 トークン以下: {}",
            d["estimated_tokens"]
        );
        // ページ天井（2,100）未満＝再退避されない。
        assert!(d["estimated_tokens"].as_u64().unwrap() < READ_TOOL_RESULT_TOKEN_LIMIT as u64);
    }

    /// テスト 6: 旧 offset / limit（バイト）は未知キーとして無視され、全文読みへ落ちる。
    #[tokio::test]
    async fn test_ws_read_legacy_offset_limit_falls_to_full_read() {
        let (_dir, ctx) = test_context();
        WsWriteAction
            .execute(&json!({"path": "f.txt", "content": "l1\nl2\nl3"}), &ctx)
            .await;

        let r = WsReadAction
            .execute(&json!({"path": "f.txt", "offset": 3, "limit": 4}), &ctx)
            .await;
        let d = r.data.unwrap();
        // 未知キーは無視される。#707 で経路が 1 本になったので、3 行のファイルは
        // 1 ページ目に全部入り、続きは無い（行メタは付く＝ページとして返るため）。
        assert_eq!(d["content"].as_str(), Some("l1\nl2\nl3"));
        assert_eq!(d["has_more"].as_bool(), Some(false));
        assert!(
            d.get("next_line").is_none(),
            "続きが無ければ next_line も無い"
        );
    }

    /// 512 文字切り＋**標識付き**（overflow > 0）でも、返りページの推定トークンは
    /// RANGE_CONTENT_TOKEN_CEILING(2,100) 未満に収まる（＝再退避しない）。overflow=0 の 512
    /// ちょうどだけでなく、標識込みの最悪ケースも天井内であることを固定する。
    #[tokio::test]
    async fn test_ws_read_truncated_marked_line_under_ceiling() {
        let (_dir, ctx) = test_context();
        // 4 バイト文字を 2,500 文字。2,000 で切られ overflow=500 の標識が付く＝最悪密度＋標識
        // （#707 で 1 行上限が 2,000 文字になったので、素材もそれを超える必要がある）。
        let dense = "𠀀".repeat(2_500);
        WsWriteAction
            .execute(&json!({"path": "d.txt", "content": dense}), &ctx)
            .await;

        let r = WsReadAction
            .execute(&json!({"path": "d.txt", "start_line": 1}), &ctx)
            .await;
        let d = r.data.unwrap();
        let content = d["content"].as_str().unwrap();
        assert!(
            content.contains("…⟨+500文字⟩"),
            "切られた標識が付く: {content:.64}"
        );
        assert!(
            d["estimated_tokens"].as_u64().unwrap() < RANGE_CONTENT_TOKEN_CEILING as u64,
            "標識込みでもページ天井（読み上限−400）未満: {}",
            d["estimated_tokens"]
        );
    }

    /// テスト 3: 大きなファイルでも行範囲指定なら返す本文は inline 上限を超えず、退避
    /// （自己ループ / #564）を断つ。範囲指定なしの全文は上限を超え、退避される旨のヒントが付く。
    #[tokio::test]
    async fn test_ws_read_line_range_stays_under_inline_limit() {
        let (_dir, ctx) = test_context();
        // 読みの上限（30,000 tok / #707）を確実に超える大きさ。1 行 80 文字 × 5,000 行 ≒ 10 万トークン。
        let line = "x".repeat(80);
        let big: String = std::iter::repeat_n(line.as_str(), 5_000)
            .collect::<Vec<_>>()
            .join("\n");
        WsWriteAction
            .execute(&json!({"path": "big.txt", "content": big}), &ctx)
            .await;

        // #707: 範囲指定なしも 1 ページ目として返す（全文を返して退避する経路は廃止）。
        let full = WsReadAction
            .execute(&json!({"path": "big.txt"}), &ctx)
            .await;
        let fd = full.data.unwrap();
        assert!(
            fd["estimated_tokens"].as_u64().unwrap() <= READ_TOOL_RESULT_TOKEN_LIMIT as u64,
            "範囲指定なしでも上限内（＝退避されない）: {}",
            fd["estimated_tokens"]
        );
        assert_eq!(
            fd["has_more"].as_bool(),
            Some(true),
            "10 万トークンのファイルは 1 ページに収まらないので続きがある"
        );

        // 行範囲指定あり: 返す本文は上限未満（＝退避されない＝自己ループが起きない）。
        let ranged = WsReadAction
            .execute(&json!({"path": "big.txt", "start_line": 1}), &ctx)
            .await;
        let rd = ranged.data.unwrap();
        assert!(
            rd["estimated_tokens"].as_u64().unwrap() <= READ_TOOL_RESULT_TOKEN_LIMIT as u64,
            "行範囲読みの本文は inline 上限を超えない"
        );
        assert_eq!(
            rd["has_more"].as_bool(),
            Some(true),
            "予算で頭打ち＝続きがある"
        );
        assert!(rd.get("hint").is_none(), "範囲指定時はヒントを出さない");
    }

    /// テスト 4: 密テキスト（日本語, 1 文字 ≒ 1 トークン）の複数行で、ページはトークン天井
    /// （[`RANGE_CONTENT_TOKEN_CEILING`] ≒ 2,100）に達する直前で止まる。返り本文は inline 上限を
    /// 超えず（再退避しない）、next_line で続きを辿れる。
    #[tokio::test]
    async fn test_ws_read_page_ceiling_binds_on_dense_text() {
        let (_dir, ctx) = test_context();
        // 1 行 100 文字の日本語 × 400 行（総計 ~40,000 トークン ≫ ページ天井 29,600）。
        // #707 で天井が上がったので、天井が効くことを見るには素材もそれを超える必要がある
        // （100 行 ≒ 1 万トークンは今や 1 回で読める＝それがこの修正の狙い）。
        let line = "あ".repeat(100);
        let big: String = std::iter::repeat_n(line.as_str(), 400)
            .collect::<Vec<_>>()
            .join("\n");
        WsWriteAction
            .execute(&json!({"path": "jp.txt", "content": big}), &ctx)
            .await;

        let r = WsReadAction
            .execute(&json!({"path": "jp.txt", "start_line": 1}), &ctx)
            .await;
        let d = r.data.unwrap();
        assert!(
            d["estimated_tokens"].as_u64().unwrap() <= READ_TOOL_RESULT_TOKEN_LIMIT as u64,
            "日本語でもページ本文は inline 上限を超えない（自己ループ防止）"
        );
        // 400 行すべては載らず、天井で切られて続きの導線が付く。
        assert_eq!(d["has_more"].as_bool(), Some(true));
        let next = d["next_line"].as_u64().unwrap();
        assert!(
            next > 1 && next <= 400,
            "next_line が範囲内で前進する: {next}"
        );
    }

    /// テスト 5: #567: 行範囲読みは同期 CPU を `spawn_blocking` に逃がすので、単一スレッド
    /// runtime でも executor（他タスク）を塞がない。裏で 1ms ごとに進むタスクが read 中も前進する
    /// ことを見る。range logic が execute 内でインライン実行されていれば、単一スレッド runtime では
    /// このカウンタは read が終わるまで 1 も進まない（インライン化への変異検出）。
    #[tokio::test]
    async fn test_ws_read_does_not_block_executor() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc as StdArc;

        let (_dir, ctx) = test_context();
        // そこそこ大きい（=読み取り＋トークナイズに実 CPU 時間がかかる）ファイル（1 行 80 文字）。
        let line = "x".repeat(80);
        let big: String = std::iter::repeat_n(line.as_str(), 25_000)
            .collect::<Vec<_>>()
            .join("\n");
        WsWriteAction
            .execute(&json!({"path": "big.txt", "content": big}), &ctx)
            .await;

        let ticks = StdArc::new(AtomicU64::new(0));
        let ticks2 = ticks.clone();
        let ticker = tokio::spawn(async move {
            for _ in 0..1000 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                ticks2.fetch_add(1, Ordering::Relaxed);
            }
        });

        let r = WsReadAction
            .execute(&json!({"path": "big.txt", "start_line": 1}), &ctx)
            .await;
        assert!(r.success);
        // read の await 中に executor が ticker を進められていれば > 0。spawn_blocking に
        // 入っていない（インライン CPU）と、単一スレッド runtime では 0 のまま。
        assert!(
            ticks.load(Ordering::Relaxed) > 0,
            "read 中も別タスクが前進する（executor を塞いでいない）"
        );
        ticker.abort();
    }
}
