use async_trait::async_trait;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult};

/// 記憶インデックスのツリー構造を閲覧するアクション
pub struct BrowseMemoryIndexAction;

#[async_trait]
impl Action for BrowseMemoryIndexAction {
    fn name(&self) -> &str {
        "browse_memory_index"
    }

    fn description(&self) -> &str {
        "記憶インデックスのツリー構造を閲覧する。タイトルと要約のみのコンパクト表示。関連しそうなノードを見つけたら retrieve_memory_nodes で全文を取得できる。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "max_depth": {
                    "type": "integer",
                    "description": "表示する最大深さ（デフォルト: 3）",
                    "default": 3
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let max_depth = args["max_depth"].as_i64().unwrap_or(3) as i32;

        let tree = if let Ok(conn) = ctx.db.lock() {
            match opencrab_db::queries::get_index_tree(&conn, &ctx.agent_id) {
                Ok(nodes) => nodes,
                Err(e) => return ActionResult::error(&format!("Failed to get index tree: {e}")),
            }
        } else {
            return ActionResult::error("Failed to acquire DB lock");
        };

        let filtered: Vec<serde_json::Value> = tree
            .iter()
            .filter(|n| n.depth <= max_depth)
            .map(|n| {
                json!({
                    "node_id": n.id,
                    "short_id": n.short_id,
                    "parent_id": n.parent_id,
                    "node_type": n.node_type,
                    "title": n.title,
                    "summary": n.summary,
                    "keywords": parse_keywords(&n.keywords_json),
                    "depth": n.depth,
                    "child_count": n.child_count,
                    "start_log_id": n.start_log_id,
                    "end_log_id": n.end_log_id,
                })
            })
            .collect();

        ActionResult::success(json!({
            "node_count": filtered.len(),
            "tree": filtered,
        }))
    }
}

/// 記憶インデックスノードの全文テキストを取得するアクション
pub struct RetrieveMemoryNodesAction;

#[async_trait]
impl Action for RetrieveMemoryNodesAction {
    fn name(&self) -> &str {
        "retrieve_memory_nodes"
    }

    fn description(&self) -> &str {
        "browse_memory_indexで見つけたノードの全文テキストを取得する。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["node_ids"],
            "properties": {
                "node_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "取得するノードIDのリスト（1-5個）",
                    "minItems": 1,
                    "maxItems": 5
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let node_ids: Vec<String> = match args["node_ids"].as_array() {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            None => return ActionResult::error("node_ids is required (array of strings)"),
        };

        if node_ids.is_empty() {
            return ActionResult::error("node_ids must not be empty");
        }
        if node_ids.len() > 5 {
            return ActionResult::error("Maximum 5 node_ids allowed");
        }

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };

        let mut results: Vec<serde_json::Value> = Vec::new();

        for node_id in &node_ids {
            let node = match opencrab_db::queries::get_index_node_by_short_or_id(
                &conn,
                &ctx.agent_id,
                node_id,
            ) {
                Ok(Some(n)) => n,
                Ok(None) => {
                    results.push(json!({
                        "node_id": node_id,
                        "error": "Node not found",
                    }));
                    continue;
                }
                Err(e) => {
                    results.push(json!({
                        "node_id": node_id,
                        "error": format!("Query error: {e}"),
                    }));
                    continue;
                }
            };

            // ログIDレンジがある場合は全文取得
            let messages = if let (Some(start), Some(end)) = (node.start_log_id, node.end_log_id) {
                match opencrab_db::queries::get_session_logs_by_id_range(
                    &conn,
                    &ctx.agent_id,
                    start,
                    end,
                ) {
                    Ok(logs) => logs
                        .iter()
                        .map(|l| {
                            json!({
                                "speaker": l.speaker_id.as_deref().unwrap_or("unknown"),
                                "content": l.content,
                                "log_type": l.log_type,
                            })
                        })
                        .collect::<Vec<_>>(),
                    Err(_) => vec![],
                }
            } else {
                vec![]
            };

            results.push(json!({
                "node_id": node.id,
                "title": node.title,
                "summary": node.summary,
                "node_type": node.node_type,
                "keywords": parse_keywords(&node.keywords_json),
                "messages": messages,
            }));
        }

        ActionResult::success(json!({
            "nodes": results,
        }))
    }
}

/// keywords_json（JSON 配列文字列）を Vec<String> にほどく（壊れていれば空）。
fn parse_keywords(keywords_json: &str) -> Vec<String> {
    serde_json::from_str(keywords_json).unwrap_or_default()
}

/// 記憶インデックスをキーワードで逆引き検索するアクション
pub struct SearchMemoryIndexAction;

#[async_trait]
impl Action for SearchMemoryIndexAction {
    fn name(&self) -> &str {
        "search_memory_index"
    }

    fn description(&self) -> &str {
        "記憶インデックスをキーワードで検索する（タイトル・要約・キーワードの全文検索）。ヒットしたノードの short_id を retrieve_memory_nodes に渡すと当時の生ログまで遡れる。生ログ自体を直接検索したい場合は search_my_history を使う。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {
                    "type": "string",
                    "description": "検索キーワード（空白区切りで複数可）"
                },
                "limit": {
                    "type": "integer",
                    "description": "最大件数（デフォルト: 10、最大: 25）",
                    "default": 10
                },
                "node_type": {
                    "type": "string",
                    "description": "ノード種別で絞り込む（topic / period / session / daily など。省略時は全種別）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let query = match args["query"].as_str() {
            Some(q) if !q.trim().is_empty() => q.trim(),
            _ => return ActionResult::error("query is required"),
        };
        let limit = args["limit"].as_i64().unwrap_or(10).clamp(1, 25) as usize;
        let node_type = args["node_type"].as_str().filter(|s| !s.is_empty());

        let results = {
            let conn = match ctx.db.lock() {
                Ok(c) => c,
                Err(_) => return ActionResult::error("Failed to acquire DB lock"),
            };
            match opencrab_db::queries::search_index_nodes(
                &conn,
                &ctx.agent_id,
                query,
                limit,
                node_type,
            ) {
                Ok(r) => r,
                Err(e) => return ActionResult::error(&format!("Search failed: {e}")),
            }
        };

        let nodes: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                json!({
                    "short_id": r.short_id,
                    "node_id": r.node_id,
                    "node_type": r.node_type,
                    "source_type": r.source_type,
                    "title": r.title,
                    "summary": r.summary,
                    "keywords": parse_keywords(&r.keywords_json),
                    "date_from": r.date_from,
                    "date_to": r.date_to,
                    "child_count": r.child_count,
                    "score": r.score,
                })
            })
            .collect();

        ActionResult::success(json!({
            "query": query,
            "count": nodes.len(),
            "nodes": nodes,
        }))
    }
}

// ============================================
// タグ操作アクション（issue #359 / #313 段階2）
// ============================================
// エージェント自身が記憶（topic）にタグを付ける道具 3 個。整理ラン（段階3・caller=Owner）
// から使う。**TRUSTED_ONLY**（`bridge::TRUSTED_ONLY_ACTIONS`）で Nostr（caller=Agent）
// からは list_tools に出ず dispatch でも拒否される。タグは `node_type='category'` の
// ノードで、一覧は `browse_memory_index` で引けるので専用の一覧アクションは作らない。

/// topic_id（short_id またはフル id）を解決してフル id を返す。member 行には join が
/// 効くフル id を格納する（`list_unassigned_topics` 等が `topic_node.id` と突き合わせる）。
fn resolve_topic_full_id(
    conn: &rusqlite::Connection,
    agent_id: &str,
    topic_id: &str,
) -> Result<String, String> {
    match opencrab_db::queries::get_index_node_by_short_or_id(conn, agent_id, topic_id) {
        Ok(Some(node)) => Ok(node.id),
        Ok(None) => Err(format!("topic '{topic_id}' が見つかりません")),
        Err(e) => Err(format!("topic の解決に失敗しました: {e}")),
    }
}

/// 記憶（topic）に複数タグを付けるアクション（多対多）。無いタグ名は同時に新設する。
pub struct TagTopicAction;

#[async_trait]
impl Action for TagTopicAction {
    fn name(&self) -> &str {
        "tag_topic"
    }

    fn description(&self) -> &str {
        "記憶インデックスの topic にタグを付ける（複数可・多対多）。無いタグ名はその場で新設される。付いたタグは browse_memory_index / search_memory_index の category ノードとして引ける。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["topic_id", "tags"],
            "properties": {
                "topic_id": {
                    "type": "string",
                    "description": "対象 topic の short_id またはフル node_id"
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "付けるタグ名の配列（無い名前は新設）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let topic_id = match args["topic_id"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("topic_id is required"),
        };
        let tags: Vec<String> = match args["tags"].as_array() {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            None => return ActionResult::error("tags is required (array of strings)"),
        };
        if tags.is_empty() {
            return ActionResult::error("tags must contain at least one non-empty tag");
        }

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        let full_id = match resolve_topic_full_id(&conn, &ctx.agent_id, &topic_id) {
            Ok(id) => id,
            Err(e) => return ActionResult::error(&e),
        };
        let now = chrono::Utc::now().to_rfc3339();
        // タグ新設が黙って失敗しないよう、DB 層が read-back で検証し失敗を Err にする（#359）。
        match opencrab_db::queries::tag_topic(&conn, &ctx.agent_id, &full_id, &tags, &now) {
            Ok(()) => ActionResult::success(json!({
                "topic_id": full_id,
                "tags": tags,
            })),
            Err(e) => ActionResult::error(&format!("タグ付けに失敗しました: {e}")),
        }
    }
}

/// 記憶（topic）からタグ 1 個の付与を取り消すアクション。
pub struct UntagTopicAction;

#[async_trait]
impl Action for UntagTopicAction {
    fn name(&self) -> &str {
        "untag_topic"
    }

    fn description(&self) -> &str {
        "記憶インデックスの topic からタグ 1 個の付与を外す。タグノード自体は消さない（他の topic にまだ付いているかもしれない）。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["topic_id", "tag"],
            "properties": {
                "topic_id": {
                    "type": "string",
                    "description": "対象 topic の short_id またはフル node_id"
                },
                "tag": {
                    "type": "string",
                    "description": "外すタグ名"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let topic_id = match args["topic_id"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("topic_id is required"),
        };
        let tag = match args["tag"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("tag is required"),
        };

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        let full_id = match resolve_topic_full_id(&conn, &ctx.agent_id, &topic_id) {
            Ok(id) => id,
            Err(e) => return ActionResult::error(&e),
        };
        match opencrab_db::queries::remove_tag_member(&conn, &ctx.agent_id, &full_id, &tag) {
            Ok(removed) => ActionResult::success(json!({
                "topic_id": full_id,
                "tag": tag,
                "removed": removed,
            })),
            Err(e) => ActionResult::error(&format!("タグ外しに失敗しました: {e}")),
        }
    }
}

/// タグを統合するアクション（member を付け替え、from ノードを削除）。
pub struct MergeTagsAction;

#[async_trait]
impl Action for MergeTagsAction {
    fn name(&self) -> &str {
        "merge_tags"
    }

    fn description(&self) -> &str {
        "2 つのタグを統合する。from タグの付与を全て into タグへ付け替え、from タグノードを削除する。into が無ければ新設する（実質リネームにもなる）。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["from", "into"],
            "properties": {
                "from": {
                    "type": "string",
                    "description": "統合元タグ名（付け替え後に削除される）"
                },
                "into": {
                    "type": "string",
                    "description": "統合先タグ名（無ければ新設）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let from = match args["from"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("from is required"),
        };
        let into = match args["into"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return ActionResult::error("into is required"),
        };

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };
        let now = chrono::Utc::now().to_rfc3339();
        match opencrab_db::queries::merge_tags(&conn, &ctx.agent_id, &from, &into, &now) {
            Ok(outcome) => ActionResult::success(json!({
                "from": from,
                "into": into,
                "moved": outcome.moved,
                "into_category_id": outcome.into_category_id,
            })),
            Err(e) => ActionResult::error(&format!("タグ統合に失敗しました: {e}")),
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

    // ---- BrowseMemoryIndexAction ----

    #[tokio::test]
    async fn test_browse_memory_index_empty() {
        let (_dir, ctx) = test_context();
        let result = BrowseMemoryIndexAction.execute(&json!({}), &ctx).await;
        assert!(result.success);
        let data = result.data.unwrap();
        assert_eq!(data["node_count"], 0);
    }

    #[tokio::test]
    async fn test_browse_memory_index_with_nodes() {
        let (_dir, ctx) = test_context();
        {
            let conn = ctx.db.lock().unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            let node = opencrab_db::queries::IndexNodeRow {
                id: "root-agent-1".to_string(),
                agent_id: "agent-1".to_string(),
                parent_id: None,
                node_type: "root".to_string(),
                source_type: "session_log".to_string(),
                title: "Memory Root".to_string(),
                summary: "Root node".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 1,
                token_count: 0,
                created_at: now.clone(),
                updated_at: now.clone(),
                short_id: None,
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            };
            opencrab_db::queries::insert_index_node(&conn, &node).unwrap();
            let topic = opencrab_db::queries::IndexNodeRow {
                id: "topic-1".to_string(),
                agent_id: "agent-1".to_string(),
                parent_id: Some("root-agent-1".to_string()),
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: "Rust Discussion".to_string(),
                summary: "A discussion about Rust.".to_string(),
                start_log_id: Some(1),
                end_log_id: Some(5),
                source_session_id: Some("session-1".to_string()),
                date_from: None,
                date_to: None,
                depth: 1,
                child_count: 0,
                token_count: 100,
                created_at: now.clone(),
                updated_at: now,
                short_id: None,
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            };
            opencrab_db::queries::insert_index_node(&conn, &topic).unwrap();
        }
        let result = BrowseMemoryIndexAction.execute(&json!({}), &ctx).await;
        assert!(result.success);
        let data = result.data.unwrap();
        assert_eq!(data["node_count"], 2);
        let tree = data["tree"].as_array().unwrap();
        assert!(tree.iter().any(|n| n["title"] == "Rust Discussion"));
    }

    // ---- RetrieveMemoryNodesAction ----

    #[tokio::test]
    async fn test_retrieve_memory_nodes_missing_ids() {
        let (_dir, ctx) = test_context();
        let result = RetrieveMemoryNodesAction.execute(&json!({}), &ctx).await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_retrieve_memory_nodes_not_found() {
        let (_dir, ctx) = test_context();
        let result = RetrieveMemoryNodesAction
            .execute(&json!({"node_ids": ["nonexistent"]}), &ctx)
            .await;
        assert!(result.success);
        let data = result.data.unwrap();
        let nodes = data["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0]["error"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn test_retrieve_memory_nodes_with_logs() {
        let (_dir, ctx) = test_context();
        {
            let conn = ctx.db.lock().unwrap();
            // Insert log
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "agent-1".to_string(),
                session_id: "session-1".to_string(),
                log_type: "message".to_string(),
                content: "Hello from test".to_string(),
                speaker_id: Some("user-1".to_string()),
                turn_number: Some(1),
                metadata_json: None,
                created_at: None,
            };
            opencrab_db::queries::insert_session_log(&conn, &log).unwrap();
            // Insert index node
            let now = chrono::Utc::now().to_rfc3339();
            let node = opencrab_db::queries::IndexNodeRow {
                id: "topic-test".to_string(),
                agent_id: "agent-1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: "Test Topic".to_string(),
                summary: "Test summary".to_string(),
                start_log_id: Some(1),
                end_log_id: Some(1),
                source_session_id: Some("session-1".to_string()),
                date_from: None,
                date_to: None,
                depth: 3,
                child_count: 0,
                token_count: 10,
                created_at: now.clone(),
                updated_at: now,
                short_id: None,
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            };
            opencrab_db::queries::insert_index_node(&conn, &node).unwrap();
        }
        let result = RetrieveMemoryNodesAction
            .execute(&json!({"node_ids": ["topic-test"]}), &ctx)
            .await;
        assert!(result.success);
        let data = result.data.unwrap();
        let nodes = data["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["title"], "Test Topic");
        let messages = nodes[0]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"], "Hello from test");
    }

    // ---- タグ操作アクション（#359 / #313 段階2）----

    /// テスト用に topic ノードを 1 件積む（short_id 付き）。
    fn seed_topic(ctx: &ActionContext, id: &str, short_id: &str) {
        let conn = ctx.db.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        opencrab_db::queries::insert_index_node(
            &conn,
            &opencrab_db::queries::IndexNodeRow {
                id: id.to_string(),
                agent_id: "agent-1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: format!("topic-{id}"),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: now.clone(),
                updated_at: now,
                short_id: Some(short_id.to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }

    /// tag_topic は short_id で topic を解決し、複数タグを付ける。untag で外せる。
    #[tokio::test]
    async fn test_tag_and_untag_topic_action() {
        let (_dir, ctx) = test_context();
        seed_topic(&ctx, "topic-1", "t1");

        // short_id で解決して 2 タグを付ける。
        let r = TagTopicAction
            .execute(&json!({"topic_id": "t1", "tags": ["Rust", "設計"]}), &ctx)
            .await;
        assert!(r.success, "tag_topic 失敗: {:?}", r.error);
        // member 行にはフル id が入る（join が効くため）。
        assert_eq!(r.data.unwrap()["topic_id"], "topic-1");
        {
            let conn = ctx.db.lock().unwrap();
            let total: i64 = opencrab_db::queries::count_category_members(&conn, "agent-1")
                .unwrap()
                .values()
                .sum();
            assert_eq!(total, 2, "1 topic に 2 タグ");
        }

        // 1 タグを外す。
        let r = UntagTopicAction
            .execute(&json!({"topic_id": "t1", "tag": "Rust"}), &ctx)
            .await;
        assert!(r.success);
        assert_eq!(r.data.unwrap()["removed"], true);
        {
            let conn = ctx.db.lock().unwrap();
            let total: i64 = opencrab_db::queries::count_category_members(&conn, "agent-1")
                .unwrap()
                .values()
                .sum();
            assert_eq!(total, 1, "1 タグ外れて残り 1");
        }
    }

    /// merge_tags は from を into へ付け替え、from ノードを消す。
    #[tokio::test]
    async fn test_merge_tags_action() {
        let (_dir, ctx) = test_context();
        seed_topic(&ctx, "topic-1", "t1");
        TagTopicAction
            .execute(&json!({"topic_id": "t1", "tags": ["旧"]}), &ctx)
            .await;

        let r = MergeTagsAction
            .execute(&json!({"from": "旧", "into": "新"}), &ctx)
            .await;
        assert!(r.success, "merge_tags 失敗: {:?}", r.error);
        assert_eq!(r.data.unwrap()["moved"], 1);
        let conn = ctx.db.lock().unwrap();
        assert!(
            opencrab_db::queries::get_category_node_by_title(&conn, "agent-1", "旧")
                .unwrap()
                .is_none(),
            "from タグは消える"
        );
        assert!(
            opencrab_db::queries::get_category_node_by_title(&conn, "agent-1", "新")
                .unwrap()
                .is_some(),
            "into タグは残る"
        );
    }

    /// 不明な topic_id / 引数不足はエラー（policy 拒否ではなく通常のバリデーション）。
    #[tokio::test]
    async fn test_tag_topic_validation() {
        let (_dir, ctx) = test_context();
        // topic 未存在。
        let r = TagTopicAction
            .execute(&json!({"topic_id": "nope", "tags": ["x"]}), &ctx)
            .await;
        assert!(!r.success);
        // tags 空。
        seed_topic(&ctx, "topic-1", "t1");
        let r = TagTopicAction
            .execute(&json!({"topic_id": "t1", "tags": []}), &ctx)
            .await;
        assert!(!r.success);
        // 引数不足。
        let r = UntagTopicAction
            .execute(&json!({"topic_id": "t1"}), &ctx)
            .await;
        assert!(!r.success);
        let r = MergeTagsAction.execute(&json!({"from": "a"}), &ctx).await;
        assert!(!r.success);
    }
}
