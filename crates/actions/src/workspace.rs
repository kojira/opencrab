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
         （#284/#294）ので、大きな退避ファイルは offset / limit で範囲を指定して読む。範囲を\
         指定すれば退避されずに読め、total_chars / has_more / next_offset で続きを辿れる。"
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
                    "description": "読み始める文字位置（0 始まり。省略時は先頭）。has_more が true のとき next_offset を渡すと続きを読める。",
                    "default": 0
                },
                "limit": {
                    "type": "integer",
                    "description": format!("返す最大文字数（省略時は全文）。全文が inline 上限 {TOOL_RESULT_TOKEN_LIMIT} トークンを超える大きなファイルは、範囲を指定すれば退避されずに読める。")
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let path = match args["path"].as_str() {
            Some(p) => p,
            None => return ActionResult::error("path is required"),
        };

        let content = match ctx.workspace.read(path).await {
            Ok(c) => c,
            Err(e) => return ActionResult::error(&e.to_string()),
        };

        // 全体の規模は常に返す（範囲指定の有無に関わらず、エージェントが続きの要否を判断できる）。
        let total_chars = content.chars().count();
        let total_lines = content.lines().count();

        // offset / limit のどちらかが与えられたら範囲読み。無ければ従来どおり全文（後方互換）。
        let has_range = !args["offset"].is_null() || !args["limit"].is_null();
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        // 範囲読みは「返す本文が inline 上限を超えない」ことを構造的に保証する（#564 の自己
        // ループ＝退避ファイルを丸ごと読んで再退避する、を断つ）。限度は文字数ではなくトークンで
        // 効かせる: `limit` はチャットモデルによって 1 文字 ≒ 1 トークンにもなり（日本語）、
        // 文字数だけで切ると返り値が上限を超えて結果ごと再退避され、metadata もろとも消える。
        let (slice, returned_chars, budget_trimmed) = if has_range {
            let requested = args["limit"]
                .as_u64()
                .map(|n| n as usize)
                .unwrap_or(usize::MAX);
            // 走査は offset から高々 `RANGE_SCAN_CHAR_CAP` 文字の窓だけに材料化する。#564 で実測
            // 509MB・単一行の退避ファイルがあり、全体を Vec<char> 化・トークナイズすると固まる。
            // 返り値はトークン上限で更に頭打ちになるので、窓さえ有界なら本文は必ず上限未満。
            // 文字境界で切る（マルチバイトを割らない。既存の truncate_chars と同流儀）。
            let scan_cap = requested.min(RANGE_SCAN_CHAR_CAP);
            let window: Vec<char> = content.chars().skip(offset).take(scan_cap).collect();
            // トークン上限に収まる最大文字数を二分探索（search.rs の budget 収束と同流儀）。
            let fits = |n: usize| {
                let probe: String = window[..n].iter().collect();
                opencrab_core::tokens::estimate_tokens(&probe) <= RANGE_CONTENT_TOKEN_CEILING
            };
            let (mut lo, mut hi) = (0usize, window.len());
            while lo < hi {
                let mid = (lo + hi).div_ceil(2);
                if fits(mid) {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            let s: String = window[..lo].iter().collect();
            // 要求（または残り）より手前で切れたか＝続きがある形で切り詰めたか。
            let wanted = requested.min(total_chars.saturating_sub(offset));
            (s, lo, lo < wanted)
        } else {
            (content, total_chars, false)
        };

        // 範囲読みの本文は上限内（≤ scan cap）なので厳密に測る。範囲指定なしの全文は巨大で
        // ありうる（実測 509MB）ため、そのままトークナイズすると固まる → 先頭サンプルから線形近似。
        let estimated_tokens = if returned_chars <= RANGE_SCAN_CHAR_CAP {
            opencrab_core::tokens::estimate_tokens(&slice)
        } else {
            let sample: String = slice.chars().take(RANGE_SCAN_CHAR_CAP).collect();
            let sample_tokens = opencrab_core::tokens::estimate_tokens(&sample);
            ((sample_tokens as u128 * total_chars.max(1) as u128) / RANGE_SCAN_CHAR_CAP as u128)
                as usize
        };
        let end = offset.saturating_add(returned_chars);
        let has_more = end < total_chars;

        let mut out = json!({
            "path": path,
            "content": slice,
            "total_chars": total_chars,
            "total_lines": total_lines,
            "offset": offset,
            "returned_chars": returned_chars,
            "has_more": has_more,
            "estimated_tokens": estimated_tokens,
            "inline_limit_tokens": TOOL_RESULT_TOKEN_LIMIT,
        });
        if has_more {
            out["next_offset"] = json!(end);
        }
        // 要求した limit がトークン上限で切り詰められたら、その旨と続きの導線を伝える。
        if budget_trimmed {
            out["note"] = json!(format!(
                "返り値が inline 上限 {TOOL_RESULT_TOKEN_LIMIT} トークンに収まるよう {returned_chars} \
                 文字で切りました。next_offset から続きを読めます。"
            ));
        }
        // 範囲指定なしで全文が inline 上限を超えるなら、退避される旨と範囲読みの導線を添える。
        if !has_range && estimated_tokens > TOOL_RESULT_TOKEN_LIMIT {
            out["hint"] = json!(format!(
                "全文（約 {estimated_tokens} トークン）は inline 上限 {TOOL_RESULT_TOKEN_LIMIT} を\
                 超えるため、この結果は退避されます。offset と limit を指定すれば範囲だけを退避\
                 されずに読めます（返り値は上限に収まるよう自動調整され、next_offset で続きを辿れます）。"
            ));
        }
        ActionResult::success(out)
    }
}

/// 範囲読みで返す本文のトークン上限。結果 JSON 全体（本文＋メタ情報の封筒）が inline 上限
/// [`TOOL_RESULT_TOKEN_LIMIT`] を超えて再退避される（#564 の自己ループ）ことを構造的に防ぐため、
/// 封筒ぶんの余白を引いた保守値にする。
const RANGE_CONTENT_TOKEN_CEILING: usize = TOOL_RESULT_TOKEN_LIMIT - 200;

/// 範囲読みで一度に材料化・トークナイズする最大文字数（硬い上限）。トークン上限に収まる文字数は
/// 内容次第で振れる（繰り返し文字は 1 トークンに多数入る）ため、estimate_tokens を巨大スライスに
/// 走らせない歯止め。#564 で実測 509MB・単一行の退避ファイルがあり、全体をトークナイズすると
/// 固まる。この窓を超える分は has_more / next_offset で続きとして辿る。128 KiB 相当。
const RANGE_SCAN_CHAR_CAP: usize = 131_072;

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

    /// 範囲指定なしは従来どおり全文を返す（後方互換）。規模メタ情報が付く。
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
        assert_eq!(d["total_chars"].as_u64(), Some(17));
        assert_eq!(d["total_lines"].as_u64(), Some(3));
        assert_eq!(d["has_more"].as_bool(), Some(false));
        assert!(d.get("next_offset").is_none());
    }

    /// offset / limit で範囲だけを返し、続きを辿るメタ情報が付く。
    #[tokio::test]
    async fn test_ws_read_range_returns_slice_and_paging_info() {
        let (_dir, ctx) = test_context();
        WsWriteAction
            .execute(&json!({"path": "f.txt", "content": "abcdefghij"}), &ctx)
            .await;

        // 先頭 4 文字。
        let r = WsReadAction
            .execute(&json!({"path": "f.txt", "offset": 0, "limit": 4}), &ctx)
            .await;
        assert!(r.success);
        let d = r.data.unwrap();
        assert_eq!(d["content"].as_str(), Some("abcd"));
        assert_eq!(d["total_chars"].as_u64(), Some(10));
        assert_eq!(d["returned_chars"].as_u64(), Some(4));
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
        assert_eq!(d["returned_chars"].as_u64(), Some(0));
        assert_eq!(d["has_more"].as_bool(), Some(false));
    }

    /// マルチバイトを文字境界で切る（バイト境界を割らない・パニックしない）。
    #[tokio::test]
    async fn test_ws_read_range_respects_char_boundary() {
        let (_dir, ctx) = test_context();
        WsWriteAction
            .execute(&json!({"path": "f.txt", "content": "あいうえお"}), &ctx)
            .await;

        let r = WsReadAction
            .execute(&json!({"path": "f.txt", "offset": 1, "limit": 2}), &ctx)
            .await;
        let d = r.data.unwrap();
        assert_eq!(d["content"].as_str(), Some("いう"));
        assert_eq!(d["total_chars"].as_u64(), Some(5));
        assert_eq!(d["returned_chars"].as_u64(), Some(2));
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
                &json!({"path": "big.json", "offset": 0, "limit": 4000}),
                &ctx,
            )
            .await;
        let rd = ranged.data.unwrap();
        assert_eq!(rd["returned_chars"].as_u64(), Some(4000));
        assert!(
            rd["estimated_tokens"].as_u64().unwrap() <= TOOL_RESULT_TOKEN_LIMIT as u64,
            "範囲読みの本文は inline 上限を超えない"
        );
        assert_eq!(rd["has_more"].as_bool(), Some(true));
        assert!(rd.get("hint").is_none(), "範囲指定時はヒントを出さない");
    }

    /// #564 の核（日本語）: `limit` は文字数だが、返り値は**トークン上限**で頭打ちになる。
    /// 日本語は 1 文字 ≒ 1 トークンなので、大きな文字 limit を指定しても返り値は inline 上限を
    /// 超えず（＝再退避しない）、要求より少なければ note と next_offset で続きを辿れる。
    #[tokio::test]
    async fn test_ws_read_range_token_budget_binds_on_dense_text() {
        let (_dir, ctx) = test_context();
        let big = "あ".repeat(50_000);
        WsWriteAction
            .execute(&json!({"path": "jp.json", "content": big}), &ctx)
            .await;

        // 文字数だけなら上限超過になる大きな limit を要求する。
        let r = WsReadAction
            .execute(
                &json!({"path": "jp.json", "offset": 0, "limit": 20_000}),
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
            d["returned_chars"].as_u64().unwrap() < 20_000,
            "予算で切り詰められる"
        );
        assert_eq!(d["has_more"].as_bool(), Some(true));
        assert!(d["next_offset"].as_u64().is_some());
        assert!(d.get("note").is_some(), "切り詰めたら note で知らせる");
    }
}
