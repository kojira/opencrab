use async_trait::async_trait;
use serde_json::json;

use crate::memory_units::HISTORY_RESULT_TOKEN_BUDGET;
use crate::traits::{Action, ActionContext, ActionResult, SideEffect};

/// `search_my_history` が受け付けるヒット件数の上限（clamp）。
///
/// 従来は `limit` を無制限に受けており、大きな値を渡すと生ログ本文がそのまま何十件も
/// 積まれて #294 のキャップに掛かっていた。俯瞰・関連探しの道具なので、`search_memory_index`
/// と同じ 25 件を上限にする。
const SEARCH_HIT_LIMIT_MAX: u64 = 25;

/// 1 ヒットの本文プレビューの最大文字数。
///
/// 生ログ本文をそのまま返すと 1 件で数十 KB になり、#294 のツール結果キャップで**丸ごと
/// メタ情報のスタブに差し替えられる**（宣言ランで検索結果が 544 バイトに潰れた #386）。
/// 検索は「関連する場面を特定する」道具なので、本文はヒットを見分けられる長さに切り、
/// 全文が要るなら `id` を `read_my_history(around_id=…)` に渡して読む導線にする。
const SEARCH_SNIPPET_CHARS: usize = 300;

/// 文字列を「最大 `max` 文字」で切り、切ったかどうかを返す（文字境界で切る）。
///
/// バイトではなく**文字数**で数えるのは、日本語（1 文字 3 バイト）の途中で切って壊れた
/// UTF-8 を作らないため。切ったときは末尾に省略記号を付け、切ったことを `true` で返す。
fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s.to_string(), false);
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    (out, true)
}

/// 検索結果 JSON（`{query,count,results}`）の serialize 後トークン数が `budget_tokens` に
/// 収まるよう、**スコア下位のヒットから**落とす。落とした件数を返す。
///
/// 本文をスニペット化しても、日本語のヒットは 1 件あたりのトークンが重く、上限件数
/// （[`SEARCH_HIT_LIMIT_MAX`]）ぶん積むと超えうる。`search_session_logs` は bm25 昇順
/// （小さいほど良い＝先頭が上位）で返すので、末尾（下位）から削れば上位ヒットは残る。
fn fit_hits_to_budget(
    query: &str,
    hits: &mut Vec<serde_json::Value>,
    budget_tokens: usize,
) -> usize {
    let tokens_for = |slice: &[serde_json::Value]| -> usize {
        let json = serde_json::to_string(&json!({
            "query": query,
            "count": slice.len(),
            "results": slice,
        }))
        .unwrap_or_default();
        opencrab_core::tokens::estimate_tokens(&json)
    };
    if tokens_for(hits) <= budget_tokens {
        return 0;
    }
    let all_len = hits.len();
    // 収まる最大の keep 件数を二分探索（先頭 keep 件＝上位を残す）。
    let (mut lo, mut hi) = (0usize, all_len);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if tokens_for(&hits[..mid]) <= budget_tokens {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    hits.truncate(lo);
    all_len - lo
}

/// 自分の履歴を検索するアクション
pub struct SearchMyHistoryAction;

#[async_trait]
impl Action for SearchMyHistoryAction {
    fn name(&self) -> &str {
        "search_my_history"
    }

    fn description(&self) -> &str {
        "自分の過去のやりとりを生ログから全文検索する。関連する場面の特定に使う。本文はプレビュー（先頭の一部）で返る。全文が要るときはヒットの id を read_my_history(around_id=…) に渡して読む。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {
                    "type": "string",
                    "description": "検索クエリ"
                },
                "limit": {
                    "type": "integer",
                    "description": format!("取得件数（デフォルト: 10 / 最大: {SEARCH_HIT_LIMIT_MAX}）"),
                    "default": 10
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let query = match args["query"].as_str() {
            Some(q) => q,
            None => return ActionResult::error("query is required"),
        };
        let limit = args["limit"]
            .as_u64()
            .unwrap_or(10)
            .clamp(1, SEARCH_HIT_LIMIT_MAX) as usize;

        let results = if let Ok(conn) = ctx.db.lock() {
            match opencrab_db::queries::search_session_logs(&conn, &ctx.agent_id, query, limit) {
                Ok(r) => r,
                Err(e) => return ActionResult::error(&format!("Search failed: {e}")),
            }
        } else {
            return ActionResult::error("Failed to acquire DB lock");
        };

        // 生ログ本文をそのまま返すと 1 件で数十 KB になり、#294 のキャップで丸ごと
        // スタブに潰れる（#386）。本文はヒットを見分けられる長さに切り、全文は id を
        // read_my_history(around_id) に渡して読む導線にする。
        let mut any_content_truncated = false;
        let mut hits: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                let (content, truncated) = truncate_chars(&r.content, SEARCH_SNIPPET_CHARS);
                any_content_truncated |= truncated;
                json!({
                    "id": r.id,
                    "session_id": r.session_id,
                    "log_type": r.log_type,
                    "created_at": r.created_at,
                    "score": r.score,
                    "content": content,
                    "content_truncated": truncated,
                })
            })
            .collect();

        // スニペット化しても日本語ヒットはトークンが重い。予算に収まるヒット数まで
        // スコア下位から落とす（上位は残る）。
        let dropped = fit_hits_to_budget(query, &mut hits, HISTORY_RESULT_TOKEN_BUDGET);

        let mut data = json!({
            "query": query,
            "count": hits.len(),
            "results": hits,
        });
        // 本文を切った / 件数を落としたときだけ、全文への導線を 1 行添える。
        if any_content_truncated || dropped > 0 {
            let note = if dropped > 0 {
                format!(
                    "本文はプレビュー。全文は read_my_history(around_id=<ヒットのid>) で読める。\
                     さらに下位のヒット {dropped} 件は上限内に収めるため省いた（クエリを絞ると良い）。"
                )
            } else {
                "本文はプレビュー。全文は read_my_history(around_id=<ヒットのid>) で読める。"
                    .to_string()
            };
            data["note"] = json!(note);
        }

        ActionResult::success(data)
    }
}

/// 要約して保存するアクション
pub struct SummarizeAndSaveAction;

#[async_trait]
impl Action for SummarizeAndSaveAction {
    fn name(&self) -> &str {
        "summarize_and_save"
    }

    fn description(&self) -> &str {
        "内容を要約してワークスペースに保存する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["content", "filename"],
            "properties": {
                "content": {
                    "type": "string",
                    "description": "保存する要約内容"
                },
                "filename": {
                    "type": "string",
                    "description": "保存先ファイル名（相対パス）"
                },
                "summary_type": {
                    "type": "string",
                    "enum": ["session", "topic", "research", "note"],
                    "description": "要約の種類"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let content = match args["content"].as_str() {
            Some(c) => c,
            None => return ActionResult::error("content is required"),
        };
        let filename = match args["filename"].as_str() {
            Some(f) => f,
            None => return ActionResult::error("filename is required"),
        };

        match ctx.workspace.write(filename, content).await {
            Ok(_) => ActionResult::success(json!({
                "saved": true,
                "filename": filename,
            }))
            .with_side_effect(SideEffect::FileWritten {
                path: filename.to_string(),
            }),
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

    // ---- SearchMyHistoryAction ----

    #[tokio::test]
    async fn test_search_my_history_missing_query() {
        let (_dir, ctx) = test_context();
        let result = SearchMyHistoryAction.execute(&json!({}), &ctx).await;
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("query is required"));
    }

    #[tokio::test]
    async fn test_search_my_history_empty_results() {
        let (_dir, ctx) = test_context();
        let result = SearchMyHistoryAction
            .execute(&json!({"query": "nonexistent"}), &ctx)
            .await;
        assert!(result.success);
        let data = result.data.unwrap();
        assert_eq!(data["count"], 0);
    }

    #[tokio::test]
    async fn test_search_my_history_with_data() {
        let (_dir, ctx) = test_context();
        {
            let conn = ctx.db.lock().unwrap();
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "agent-1".to_string(),
                session_id: "session-1".to_string(),
                log_type: "message".to_string(),
                content: "Rust programming is wonderful".to_string(),
                speaker_id: Some("agent-1".to_string()),
                turn_number: Some(1),
                metadata_json: None,
                created_at: None,
            };
            opencrab_db::queries::insert_session_log(&conn, &log).unwrap();
        }
        let result = SearchMyHistoryAction
            .execute(&json!({"query": "Rust", "limit": 5}), &ctx)
            .await;
        assert!(result.success);
        let data = result.data.unwrap();
        assert!(data["count"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn test_search_my_history_custom_limit() {
        let (_dir, ctx) = test_context();
        let result = SearchMyHistoryAction
            .execute(&json!({"query": "anything", "limit": 3}), &ctx)
            .await;
        assert!(result.success);
        assert_eq!(result.data.unwrap()["count"], 0);
    }

    // ---- 返り値を上限内に収める（#386）----

    fn insert_log(ctx: &ActionContext, session: &str, content: &str) {
        let conn = ctx.db.lock().unwrap();
        opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "agent-1".to_string(),
                session_id: session.to_string(),
                log_type: "speech".to_string(),
                content: content.to_string(),
                speaker_id: Some("agent-1".to_string()),
                turn_number: Some(1),
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn truncate_chars_is_char_safe() {
        // 短い → そのまま。
        let (s, t) = truncate_chars("あいう", 10);
        assert_eq!(s, "あいう");
        assert!(!t);
        // 長い → 文字境界で切って省略記号（壊れた UTF-8 を作らない）。
        let (s, t) = truncate_chars(&"あ".repeat(100), 5);
        assert!(t);
        assert_eq!(s.chars().count(), 6); // 5 文字 + …
        assert!(s.ends_with('…'));
    }

    #[test]
    fn fit_hits_to_budget_drops_low_ranked_until_it_fits() {
        // 予算を超える太いヒットを積む。
        let hits_src: Vec<serde_json::Value> = (0..25)
            .map(|i| {
                json!({
                    "id": i,
                    "content": "あ".repeat(SEARCH_SNIPPET_CHARS),
                    "content_truncated": true,
                })
            })
            .collect();
        let mut hits = hits_src.clone();
        let dropped = fit_hits_to_budget("q", &mut hits, HISTORY_RESULT_TOKEN_BUDGET);
        assert!(dropped > 0, "予算超過なら落とすはず");
        assert_eq!(hits.len() + dropped, 25);
        // 上位（先頭）を残す。
        assert_eq!(hits[0]["id"], 0);
        // 収まっている。
        let json =
            serde_json::to_string(&json!({"query":"q","count":hits.len(),"results":hits})).unwrap();
        assert!(opencrab_core::tokens::estimate_tokens(&json) <= HISTORY_RESULT_TOKEN_BUDGET);
    }

    /// 1 件の長大ログでも、本文はプレビューに切られ id で全文へ辿れる。
    #[tokio::test]
    async fn search_caps_long_content_and_points_to_read() {
        let (_dir, ctx) = test_context();
        insert_log(&ctx, "s1", &format!("keyword {}", "あ".repeat(5000)));

        let r = SearchMyHistoryAction
            .execute(&json!({"query": "keyword"}), &ctx)
            .await;
        assert!(r.success);
        let data = r.data.unwrap();
        assert_eq!(data["count"], 1);
        let hit = &data["results"][0];
        // 本文はスニペット（元 5000+ 文字が 300+1 文字に）。
        assert!(hit["content"].as_str().unwrap().chars().count() <= SEARCH_SNIPPET_CHARS + 1);
        assert_eq!(hit["content_truncated"], true);
        // 全文へ辿るための id が載る。
        assert!(hit["id"].as_i64().is_some());
        // 全文への導線 note がある。
        assert!(data["note"].as_str().unwrap().contains("read_my_history"));
    }

    /// 大量の長大ログ（生 content 合計が巨大）でも、返り値はラッパ込みで上限未満。
    #[tokio::test]
    async fn search_result_stays_under_inline_limit() {
        let (_dir, ctx) = test_context();
        for i in 0..40 {
            insert_log(
                &ctx,
                &format!("s{i}"),
                &format!("keyword {}", "設計と実装 ".repeat(400)),
            );
        }
        // limit を上限超えに指定しても clamp される。
        let r = SearchMyHistoryAction
            .execute(&json!({"query": "keyword", "limit": 1000}), &ctx)
            .await;
        assert!(r.success);
        let wrapped = serde_json::to_string(&r).unwrap();
        let tokens = opencrab_core::tokens::estimate_tokens(&wrapped);
        assert!(
            tokens < opencrab_core::tool_result_log::TOOL_RESULT_TOKEN_LIMIT,
            "search over inline limit: {tokens}"
        );
        let data = r.data.unwrap();
        // clamp（<=25）＋予算トリムで件数は絞られる。
        assert!(data["count"].as_u64().unwrap() <= 25);
        assert!(data["note"].as_str().unwrap().contains("read_my_history"));
    }

    /// 小さい結果は従来どおり本文が丸ごと載る（プレビュー切りも note も無い）。
    #[tokio::test]
    async fn search_small_result_is_full_and_unmarked() {
        let (_dir, ctx) = test_context();
        insert_log(&ctx, "s1", "keyword short body");

        let r = SearchMyHistoryAction
            .execute(&json!({"query": "keyword"}), &ctx)
            .await;
        let data = r.data.unwrap();
        assert_eq!(data["count"], 1);
        let hit = &data["results"][0];
        assert_eq!(hit["content"], "keyword short body");
        assert_eq!(hit["content_truncated"], false);
        // 切っても落としてもいないので note は付かない。
        assert!(data.get("note").is_none());
    }
}
