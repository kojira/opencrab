use async_trait::async_trait;
use opencrab_core::tool_result_log::TOOL_RESULT_TOKEN_LIMIT;
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
         （#284/#294）ので、大きな退避ファイルは offset / limit（バイト単位）で範囲を指定して読む。\
         範囲を指定すれば退避されずに読め、total_bytes / has_more / next_offset で続きを辿れる。"
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
                "offset": {
                    "type": "integer",
                    "description": "読み始めるバイト位置（0 始まり。省略時は先頭。文字境界へ丸められる）。has_more が true のとき next_offset を渡すと続きを読める。",
                    "default": 0
                },
                "limit": {
                    "type": "integer",
                    "description": format!("返す最大バイト数（省略時は全文）。全文が inline 上限 {TOOL_RESULT_TOKEN_LIMIT} トークンを超える大きなファイルは、範囲を指定すれば退避されずに読める。")
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let path = match args["path"].as_str() {
            Some(p) => p.to_string(),
            None => return ActionResult::error("path is required"),
        };
        // offset / limit のどちらかが与えられたら範囲読み。無ければ従来どおり全文（後方互換）。
        let has_range = !args["offset"].is_null() || !args["limit"].is_null();
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().map(|n| n as usize);

        // 読み取り・計数・スライス・トークナイズを **すべて `spawn_blocking` の中** で行う
        // （マルチエージェントの async executor を同期 CPU で塞がない / #567）。返すのは有界な
        // 結果 JSON だけで、巨大な全文（実測 509MB）を executor 側へ持ち帰らない。
        let ws = ctx.workspace.clone();
        match tokio::task::spawn_blocking(move || {
            compute_ws_read(&ws, &path, has_range, offset, limit)
        })
        .await
        {
            Ok(Ok(v)) => ActionResult::success(v),
            Ok(Err(e)) => ActionResult::error(&e.to_string()),
            Err(e) => ActionResult::error(&format!("read task failed: {e}")),
        }
    }
}

/// `ws_read` の本体（同期・`spawn_blocking` 内で走る）。範囲読みはファイル読み取りから
/// スライス・トークン計測まで一括で行い、有界な結果 JSON を返す。
///
/// **範囲読みは全ファイル長に対する O(n) 走査も全文読みもしない。** 規模（`total_bytes`）は
/// `metadata().len()`（O(1)）で得、窓は [`opencrab_core::workspace::Workspace::read_file_range`]
/// で offset から高々 [`RANGE_SCAN_BYTE_CAP`] バイトだけ seek 読みする（char 境界は生バイトで
/// 補正）。トークン計測もその窓にしか掛けない。これにより 509MB の退避ファイルでも 1 回あたりの
/// IO・CPU が窓サイズで頭打ちになり、ページングも全体で O(n)（毎ページ全文読み直しの O(n²) に
/// ならない / #567）。範囲指定なしの全文読みだけは従来どおり全体を読む（後方互換）。
fn compute_ws_read(
    ws: &opencrab_core::workspace::Workspace,
    path: &str,
    has_range: bool,
    offset: usize,
    limit: Option<usize>,
) -> anyhow::Result<serde_json::Value> {
    if !has_range {
        // 従来どおり全文（後方互換）。全文を読むのはこの経路だけ。巨大な全文はサンプルから
        // トークン近似（固まらないように）。
        let content = ws.read_file(path)?;
        let total_bytes = content.len();
        let estimated_tokens = opencrab_core::tokens::estimate_tokens_bounded(&content);
        let mut out = json!({
            "path": path,
            "content": content,
            "total_bytes": total_bytes,
            "offset": 0,
            "returned_bytes": total_bytes,
            "has_more": false,
            "estimated_tokens": estimated_tokens,
            "inline_limit_tokens": TOOL_RESULT_TOKEN_LIMIT,
        });
        // 全文が inline 上限を超えるなら、退避される旨と範囲読みの導線を添える。
        if estimated_tokens > TOOL_RESULT_TOKEN_LIMIT {
            out["hint"] = json!(format!(
                "全文（約 {estimated_tokens} トークン）は inline 上限 {TOOL_RESULT_TOKEN_LIMIT} を\
                 超えるため、この結果は退避されます。offset と limit（バイト）で範囲を指定すれば\
                 退避されずに読め、返り値は上限に収まるよう自動調整され next_offset で続きを辿れます。"
            ));
        }
        return Ok(out);
    }

    // 範囲読み: **ファイル全体を読まず**、offset から高々 scan cap バイトだけを seek 読みする
    // （#567: 509MB を毎ページ読み直す O(n²) を避ける）。読んだ窓の中でトークン上限に収まる最大
    // プレフィックスへ収束させる（返り値は必ず上限未満＝再退避しない / #564 の自己ループを断つ）。
    // `limit` はバイト数だが、日本語は 1 文字 ≒ 1 トークンでバイト数だけでは上限を保証できない
    // ため、トークンで頭打ちにする。
    //
    // 読み窓は `limit` と独立に常に scan cap まで読む（#567）。窓を `limit` で縮めると、`limit` が
    // 1 文字（マルチバイト）より小さいとき窓内に完全な文字が 1 つも入らず、返り 0 バイト・
    // `next_offset` が要求 offset から進まない＝同じ位置を読み続ける**暴走ページング**になる
    // （char ベースだった旧実装には無く、byte ベース化で入り込んだ退行）。窓は必ず 1 文字ぶんを
    // 含むよう固定長で読み、`limit` は「返す最大バイト数」として返り側で効かせる。
    let (bytes, total_u64) = ws.read_file_range(path, offset as u64, RANGE_SCAN_BYTE_CAP)?;
    let total_bytes = total_u64 as usize;
    let base = (offset as u64).min(total_u64) as usize;

    // 窓は生バイトなので UTF-8 の両端を補正する。先頭の継続バイト（0b10xxxxxx）を読み飛ばして
    // 文字境界へ、末尾は `from_utf8` の valid_up_to で割れた文字を落とす。
    let lead = bytes.iter().take_while(|b| (*b & 0xC0) == 0x80).count();
    let usable = &bytes[lead..];
    let valid = match std::str::from_utf8(usable) {
        Ok(s) => s,
        Err(e) => std::str::from_utf8(&usable[..e.valid_up_to()]).unwrap_or(""),
    };
    let start = base + lead;

    // 返す最大バイト数は `limit`（無ければ窓全体）。文字境界へ切り下げてから、その範囲内で
    // トークン上限に収まる最大プレフィックスへ二分探索で収束する（search.rs の budget 収束と
    // 同流儀）。トークナイズもこの cap 以内に限るので、小さい `limit` は走査も軽い。
    let mut cap = limit.unwrap_or(valid.len()).min(valid.len());
    while cap > 0 && !valid.is_char_boundary(cap) {
        cap -= 1;
    }
    let mut bounds: Vec<usize> = valid[..cap].char_indices().map(|(i, _)| i).collect();
    bounds.push(cap);
    let fits = |b: usize| {
        opencrab_core::tokens::estimate_tokens(&valid[..b]) <= RANGE_CONTENT_TOKEN_CEILING
    };
    let (mut lo, mut hi) = (0usize, bounds.len() - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fits(bounds[mid]) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut returned_bytes = bounds[lo];
    // 最低 1 文字保証（#567）: 窓に文字があるのに `limit` / トークン上限が小さすぎて返り 0 に
    // なったら、先頭 1 文字だけは返す。返り 0 のままだと `next_offset` が `start` から進まず暴走
    // ページングになる。1 文字ぶんは `limit` / トークン上限を僅かに超えうるが、上限（≒2,100 tok）
    // ＋ 1 文字 ≪ inline 上限（2,500 tok）なので結果は再退避されない。
    if returned_bytes == 0 {
        if let Some(c) = valid.chars().next() {
            returned_bytes = c.len_utf8();
        }
    }
    let slice = &valid[..returned_bytes];
    let estimated_tokens = opencrab_core::tokens::estimate_tokens(slice);
    let end = start + returned_bytes;
    let has_more = end < total_bytes;
    // 要求（または残り）より手前で切れたか＝続きがある形で切り詰めたか。
    let wanted = limit
        .unwrap_or(usize::MAX)
        .min(total_bytes.saturating_sub(start));
    let budget_trimmed = returned_bytes < wanted;

    let mut out = json!({
        "path": path,
        "content": slice,
        "total_bytes": total_bytes,
        "offset": start,
        "returned_bytes": returned_bytes,
        "has_more": has_more,
        "estimated_tokens": estimated_tokens,
        "inline_limit_tokens": TOOL_RESULT_TOKEN_LIMIT,
    });
    if has_more {
        // `next_offset` は必ず要求 offset より前進させる。通常は `end`（> offset）。窓が壊れた
        // UTF-8 末尾で 1 文字も取り出せず `end == offset` になる隅の場合だけ、読んだ窓ぶん進めて
        // 同じ位置の読み直し（暴走ページング）を断つ（#567）。
        let next = if end > offset {
            end
        } else {
            (base + bytes.len()).max(offset + 1).min(total_bytes)
        };
        out["next_offset"] = json!(next);
    }
    if budget_trimmed {
        out["note"] = json!(format!(
            "返り値が inline 上限 {TOOL_RESULT_TOKEN_LIMIT} トークンに収まるよう {returned_bytes} \
             バイトで切りました。next_offset から続きを読めます。"
        ));
    }
    Ok(out)
}

/// 範囲読みで返す本文のトークン上限。結果 JSON 全体（本文＋メタ情報の封筒）が inline 上限
/// [`TOOL_RESULT_TOKEN_LIMIT`] を超えて再退避される（#564 の自己ループ）ことを構造的に防ぐため、
/// 封筒ぶんの余白を引いた保守値にする。封筒は keys＋数値＋`path`＋`note`（日本語）で実測
/// 約 130〜170 トークン（#567 レビュー）なので、余裕を持って 400 引く（正味余白 ~230）。
const RANGE_CONTENT_TOKEN_CEILING: usize = TOOL_RESULT_TOKEN_LIMIT - 400;

/// 範囲読みで 1 回に seek 読み・トークナイズする最大バイト数（硬い上限）。窓は
/// [`read_file_range`](opencrab_core::workspace::Workspace::read_file_range) で offset から
/// この長さだけ読むので、1 回あたりの IO・CPU は全ファイル長に依らずこの窓で頭打ちになる
/// （#564 実測 509MB・単一行でも固まらない / #567 の executor ブロック解消）。
///
/// 値は 32 KiB。返り値はさらにトークン上限（[`RANGE_CONTENT_TOKEN_CEILING`] ≒ 2,100 tok）で
/// 頭打ちになる。通常のテキストは 2〜4 バイト/トークンで 2,100 tok ≒ 4〜8 KiB なので 32 KiB
/// あれば 1 ページに満額載る。二分探索が estimate_tokens を掛けるスライスもこの窓以下に収まり、
/// 巨大窓の無駄なトークナイズを避ける。窓に収まらない分は has_more / next_offset で辿る。
const RANGE_SCAN_BYTE_CAP: usize = 32_768;

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
        assert_eq!(d["returned_bytes"].as_u64(), Some(17));
        assert_eq!(d["has_more"].as_bool(), Some(false));
        assert!(d.get("next_offset").is_none());
    }

    /// offset / limit（バイト）で範囲だけを返し、続きを辿るメタ情報が付く。
    #[tokio::test]
    async fn test_ws_read_range_returns_slice_and_paging_info() {
        let (_dir, ctx) = test_context();
        WsWriteAction
            .execute(&json!({"path": "f.txt", "content": "abcdefghij"}), &ctx)
            .await;

        // 先頭 4 バイト。
        let r = WsReadAction
            .execute(&json!({"path": "f.txt", "offset": 0, "limit": 4}), &ctx)
            .await;
        assert!(r.success);
        let d = r.data.unwrap();
        assert_eq!(d["content"].as_str(), Some("abcd"));
        assert_eq!(d["total_bytes"].as_u64(), Some(10));
        assert_eq!(d["returned_bytes"].as_u64(), Some(4));
        assert_eq!(d["has_more"].as_bool(), Some(true));
        assert_eq!(d["next_offset"].as_u64(), Some(4));

        // next_offset から続きを読む。
        let r2 = WsReadAction
            .execute(&json!({"path": "f.txt", "offset": 4, "limit": 4}), &ctx)
            .await;
        let d2 = r2.data.unwrap();
        assert_eq!(d2["content"].as_str(), Some("efgh"));
        assert_eq!(d2["has_more"].as_bool(), Some(true));
        assert_eq!(d2["next_offset"].as_u64(), Some(8));
    }

    /// offset がファイル末尾を越えたら空・has_more=false（無限ページングにならない）。
    #[tokio::test]
    async fn test_ws_read_offset_past_end_is_empty() {
        let (_dir, ctx) = test_context();
        WsWriteAction
            .execute(&json!({"path": "f.txt", "content": "abc"}), &ctx)
            .await;

        let r = WsReadAction
            .execute(&json!({"path": "f.txt", "offset": 100, "limit": 10}), &ctx)
            .await;
        let d = r.data.unwrap();
        assert_eq!(d["content"].as_str(), Some(""));
        assert_eq!(d["returned_bytes"].as_u64(), Some(0));
        assert_eq!(d["has_more"].as_bool(), Some(false));
    }

    /// バイト offset/limit がマルチバイト文字を割らない（文字境界へ丸める・パニックしない）。
    /// "あいうえお" は 1 文字 3 バイト。offset=3 は 2 文字目の先頭、limit=6 で 2 文字返る。
    #[tokio::test]
    async fn test_ws_read_range_respects_char_boundary() {
        let (_dir, ctx) = test_context();
        WsWriteAction
            .execute(&json!({"path": "f.txt", "content": "あいうえお"}), &ctx)
            .await;

        // 文字境界に載る offset/limit。
        let r = WsReadAction
            .execute(&json!({"path": "f.txt", "offset": 3, "limit": 6}), &ctx)
            .await;
        let d = r.data.unwrap();
        assert_eq!(d["content"].as_str(), Some("いう"));
        assert_eq!(d["total_bytes"].as_u64(), Some(15));
        assert_eq!(d["returned_bytes"].as_u64(), Some(6));

        // 文字の途中に落ちる offset(=4)/limit(=5) は境界へ丸められ、割れた文字を出さない。
        let r2 = WsReadAction
            .execute(&json!({"path": "f.txt", "offset": 4, "limit": 5}), &ctx)
            .await;
        let d2 = r2.data.unwrap();
        // offset は次境界(6)へ、末尾は前境界へ丸められる（"う" だけ、または "うえ" のいずれも
        // 文字として妥当）。少なくとも割れた（不正 UTF-8）文字は出ない＝as_str が取れる。
        assert!(d2["content"].as_str().is_some(), "割れた文字を返さない");
        assert!(
            d2["offset"].as_u64().unwrap() >= 6,
            "offset は文字境界へ丸められる"
        );
    }

    /// #567: `limit` が 1 文字（マルチバイト）より小さくても、範囲読みは最低 1 文字を返し
    /// `next_offset` は必ず要求 offset より前進する。返り 0・`next_offset` 据え置きだと同じ位置を
    /// 読み続ける暴走ページングになる（byte ベース化で入り込んだ退行の回帰テスト）。
    #[tokio::test]
    async fn test_ws_read_range_tiny_limit_still_advances() {
        let (_dir, ctx) = test_context();
        WsWriteAction
            .execute(&json!({"path": "jp.txt", "content": "あいう"}), &ctx)
            .await;

        // "あ" は 3 バイト。limit=1/2 は 1 文字未満だが、空返し・据え置きにならない。
        for limit in [1u64, 2] {
            let r = WsReadAction
                .execute(
                    &json!({"path": "jp.txt", "offset": 0, "limit": limit}),
                    &ctx,
                )
                .await;
            assert!(r.success);
            let d = r.data.unwrap();
            assert_eq!(
                d["content"].as_str(),
                Some("あ"),
                "limit={limit}: 最低 1 文字は返す"
            );
            assert_eq!(d["returned_bytes"].as_u64(), Some(3));
            assert_eq!(d["has_more"].as_bool(), Some(true));
            assert!(
                d["next_offset"].as_u64().unwrap() > 0,
                "limit={limit}: next_offset が要求 offset(0) から前進する（暴走ページング防止）"
            );
        }
    }

    /// #564 の核: 大きなファイルでも範囲指定なら inline 上限を超えず、退避（自己ループ）を断つ。
    /// 範囲指定なしの全文は上限を超え、退避される旨のヒントが付く。
    #[tokio::test]
    async fn test_ws_read_range_stays_under_inline_limit() {
        let (_dir, ctx) = test_context();
        // inline 上限（2,500 tok）を確実に超える大きさ。
        let big = "x".repeat(200_000);
        WsWriteAction
            .execute(&json!({"path": "big.json", "content": big}), &ctx)
            .await;

        // 範囲指定なし: 全文が返り、上限超過のヒントが付く。
        let full = WsReadAction
            .execute(&json!({"path": "big.json"}), &ctx)
            .await;
        let fd = full.data.unwrap();
        assert!(fd["estimated_tokens"].as_u64().unwrap() > TOOL_RESULT_TOKEN_LIMIT as u64);
        assert!(fd.get("hint").is_some(), "全文が上限超過ならヒントを出す");

        // 範囲指定あり: 返す本文は上限未満（＝退避されない＝自己ループが起きない）。
        let ranged = WsReadAction
            .execute(
                &json!({"path": "big.json", "offset": 0, "limit": 100_000}),
                &ctx,
            )
            .await;
        let rd = ranged.data.unwrap();
        assert!(
            rd["estimated_tokens"].as_u64().unwrap() <= TOOL_RESULT_TOKEN_LIMIT as u64,
            "範囲読みの本文は inline 上限を超えない"
        );
        assert!(
            rd["returned_bytes"].as_u64().unwrap() < 100_000,
            "予算で頭打ちになる"
        );
        assert_eq!(rd["has_more"].as_bool(), Some(true));
        assert!(rd.get("hint").is_none(), "範囲指定時はヒントを出さない");
    }

    /// #564 の核（日本語）: `limit` はバイト数だが、返り値は**トークン上限**で頭打ちになる。
    /// 日本語は 1 文字 ≒ 1 トークンなので、大きなバイト limit を指定しても返り値は inline 上限を
    /// 超えず（＝再退避しない）、要求より少なければ note と next_offset で続きを辿れる。
    #[tokio::test]
    async fn test_ws_read_range_token_budget_binds_on_dense_text() {
        let (_dir, ctx) = test_context();
        let big = "あ".repeat(50_000); // 150,000 バイト
        WsWriteAction
            .execute(&json!({"path": "jp.json", "content": big}), &ctx)
            .await;

        // バイト数だけなら上限超過になる大きな limit を要求する。
        let r = WsReadAction
            .execute(
                &json!({"path": "jp.json", "offset": 0, "limit": 60_000}),
                &ctx,
            )
            .await;
        let d = r.data.unwrap();
        assert!(
            d["estimated_tokens"].as_u64().unwrap() <= TOOL_RESULT_TOKEN_LIMIT as u64,
            "日本語でも範囲読みの本文は inline 上限を超えない（自己ループ防止）"
        );
        // トークン上限で要求 limit より手前で切られ、続きの導線が付く。
        assert!(
            d["returned_bytes"].as_u64().unwrap() < 60_000,
            "予算で切り詰められる"
        );
        assert_eq!(d["has_more"].as_bool(), Some(true));
        assert!(d["next_offset"].as_u64().is_some());
        assert!(d.get("note").is_some(), "切り詰めたら note で知らせる");
    }

    /// #567: 範囲読みは同期 CPU を `spawn_blocking` に逃がすので、単一スレッド runtime でも
    /// executor（他タスク）を塞がない。裏で 1ms ごとに進むタスクが、read 中も前進することを見る。
    /// もし range logic が execute 内でインライン実行されていれば、単一スレッド runtime では
    /// このカウンタは read が終わるまで 1 も進まない（インライン化への変異検出）。
    #[tokio::test]
    async fn test_ws_read_does_not_block_executor() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc as StdArc;

        let (_dir, ctx) = test_context();
        // そこそこ大きい（=読み取り＋トークナイズに実 CPU 時間がかかる）ファイル。
        WsWriteAction
            .execute(
                &json!({"path": "big.json", "content": "x".repeat(2_000_000)}),
                &ctx,
            )
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
            .execute(
                &json!({"path": "big.json", "offset": 0, "limit": 50_000}),
                &ctx,
            )
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
