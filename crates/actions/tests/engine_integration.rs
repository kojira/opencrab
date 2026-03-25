//! Integration test: SkillEngine → BridgedExecutor → ActionDispatcher → real Actions
//!
//! Uses MockLlm (no API keys needed). Validates that the SkillEngine can
//! drive real actions (search history, create skills, learn from experience)
//! through the BridgedExecutor bridge.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opencrab_actions::bridge::BridgedExecutor;
use opencrab_actions::dispatcher::ActionDispatcher;
use opencrab_actions::traits::{ActionContext, CallerIdentity};
use opencrab_core::{ChatRequestSimple, ChatResponseSimple, LlmClient, SkillEngine, ToolCall};

// ---------------------------------------------------------------------------
// MockLlm — returns pre-queued responses in order
// ---------------------------------------------------------------------------

struct MockLlm {
    responses: Mutex<Vec<ChatResponseSimple>>,
}

impl MockLlm {
    fn new(responses: Vec<ChatResponseSimple>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmClient for MockLlm {
    async fn chat(&self, _req: ChatRequestSimple) -> anyhow::Result<ChatResponseSimple> {
        let mut rs = self.responses.lock().unwrap();
        if rs.is_empty() {
            anyhow::bail!("MockLlm: no more responses");
        }
        Ok(rs.remove(0))
    }
}

// ---------------------------------------------------------------------------
// Helper: create a BridgedExecutor backed by in-memory DB
// ---------------------------------------------------------------------------

fn setup() -> (tempfile::TempDir, BridgedExecutor) {
    let conn = opencrab_db::init_memory().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();

    let ctx = ActionContext {
        caller: CallerIdentity::Owner,
        agent_id: "agent-1".to_string(),
        agent_name: "Test Agent".to_string(),
        session_id: Some("session-1".to_string()),
        db: Arc::new(Mutex::new(conn)),
        workspace: Arc::new(ws),
        last_metrics_id: Arc::new(Mutex::new(None)),
        model_override: Arc::new(Mutex::new(None)),
        current_purpose: Arc::new(Mutex::new("conversation".to_string())),
        runtime_info: Arc::new(Mutex::new(opencrab_actions::RuntimeInfo {
            default_model: "mock:test-model".to_string(),
            active_model: None,
            available_providers: vec!["mock".to_string()],
            gateway: "test".to_string(),
        })),
    };

    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
    (dir, executor)
}

fn setup_with_data() -> (
    tempfile::TempDir,
    BridgedExecutor,
    Arc<Mutex<rusqlite::Connection>>,
) {
    let conn = opencrab_db::init_memory().unwrap();

    // Seed a session log so search_my_history can find it
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

    let db = Arc::new(Mutex::new(conn));
    let dir = tempfile::TempDir::new().unwrap();
    let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();

    let ctx = ActionContext {
        caller: CallerIdentity::Owner,
        agent_id: "agent-1".to_string(),
        agent_name: "Test Agent".to_string(),
        session_id: Some("session-1".to_string()),
        db: Arc::clone(&db),
        workspace: Arc::new(ws),
        last_metrics_id: Arc::new(Mutex::new(None)),
        model_override: Arc::new(Mutex::new(None)),
        current_purpose: Arc::new(Mutex::new("conversation".to_string())),
        runtime_info: Arc::new(Mutex::new(opencrab_actions::RuntimeInfo {
            default_model: "mock:test-model".to_string(),
            active_model: None,
            available_providers: vec!["mock".to_string()],
            gateway: "test".to_string(),
        })),
    };

    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
    (dir, executor, db)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Engine calls search_my_history → create_my_skill → returns text.
#[tokio::test]
async fn test_engine_search_then_create_skill() {
    let (_dir, executor, db) = setup_with_data();

    let llm = MockLlm::new(vec![
        // Step 1: LLM calls search_my_history
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-1".to_string(),
                name: "search_my_history".to_string(),
                arguments: serde_json::json!({"query": "Rust"}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        // Step 2: LLM calls create_my_skill based on search results
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-2".to_string(),
                name: "create_my_skill".to_string(),
                arguments: serde_json::json!({
                    "name": "Rust Expertise",
                    "description": "Knowledge about Rust programming",
                    "situation_pattern": "when discussing Rust",
                    "guidance": "Share detailed Rust knowledge"
                }),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        // Step 3: Final text response
        ChatResponseSimple {
            content: Some(
                "I searched my history and created a new skill based on my Rust knowledge."
                    .to_string(),
            ),
            tool_calls: vec![],
            finish_reason: "stop".to_string(),
            usage: None,
        },
    ]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    let result = engine
        .run(
            "You are a learning agent",
            "Review your history and create a skill",
            "mock-model",
        )
        .await
        .unwrap();

    assert_eq!(result.iterations, 3);
    assert_eq!(result.tool_calls_made, 2);
    assert!(!result.stopped_by_limit);
    assert!(result.response.contains("skill"));

    // Verify the skill was actually persisted in the DB
    let conn = db.lock().unwrap();
    let skills = opencrab_db::queries::list_skills(&conn, "agent-1", false).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "Rust Expertise");
    assert_eq!(skills[0].source_type, "self_created");
}

/// Engine calls learn_from_experience → returns text. Verify DB skill insertion.
#[tokio::test]
async fn test_engine_learn_from_experience() {
    let (_dir, executor, db) = setup_with_data();

    let llm = MockLlm::new(vec![
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-1".to_string(),
                name: "learn_from_experience".to_string(),
                arguments: serde_json::json!({
                    "experience": "Helped user debug a complex issue",
                    "outcome": "success",
                    "lesson": "Ask for error messages first",
                    "skill_name": "debug_workflow",
                    "situation_pattern": "when user reports a bug",
                    "guidance": "Request stack trace before suggesting fixes"
                }),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        ChatResponseSimple {
            content: Some("I've learned a new debugging workflow skill.".to_string()),
            tool_calls: vec![],
            finish_reason: "stop".to_string(),
            usage: None,
        },
    ]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    let result = engine
        .run("You are a learning agent", "Learn from this", "mock-model")
        .await
        .unwrap();

    assert_eq!(result.iterations, 2);
    assert_eq!(result.tool_calls_made, 1);

    // Verify the skill was persisted
    let conn = db.lock().unwrap();
    let skills = opencrab_db::queries::list_skills(&conn, "agent-1", false).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "debug_workflow");
    assert_eq!(skills[0].source_type, "experience");
}

/// BridgedExecutor.list_tools() returns all registered actions.
#[tokio::test]
async fn test_engine_lists_all_tools() {
    let (_dir, executor) = setup();

    use opencrab_core::ActionExecutor;
    let tools = executor.list_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

    assert!(
        names.contains(&"search_my_history"),
        "missing search_my_history"
    );
    assert!(
        names.contains(&"create_my_skill"),
        "missing create_my_skill"
    );
    assert!(
        names.contains(&"learn_from_experience"),
        "missing learn_from_experience"
    );
    assert!(
        names.contains(&"learn_from_peer"),
        "missing learn_from_peer"
    );
    assert!(
        names.contains(&"reflect_and_learn"),
        "missing reflect_and_learn"
    );
    assert!(names.contains(&"send_speech"), "missing send_speech");
    assert!(names.contains(&"ws_read"), "missing ws_read");
    assert!(tools.len() >= 20, "expected 20+ tools, got {}", tools.len());
}

// ---------------------------------------------------------------------------
// Agentic RAG: browse_memory_index → retrieve_memory_nodes の2ステップ検索
// ---------------------------------------------------------------------------

/// ログ投入 + インデックス構築のセットアップヘルパー（async版）
async fn setup_with_indexed_memory() -> (
    tempfile::TempDir,
    BridgedExecutor,
    Arc<Mutex<rusqlite::Connection>>,
) {
    let conn = opencrab_db::init_memory().unwrap();

    // 3つのセッションに異なるトピックのログを投入
    let sessions = vec![
        (
            "session-rust",
            vec![
                ("user-1", "Rustのライフタイムについて教えてください"),
                (
                    "agent-1",
                    "ライフタイムは参照の有効期間を示すアノテーションです",
                ),
                ("user-1", "借用チェッカーとの関係は？"),
                (
                    "agent-1",
                    "借用チェッカーがライフタイムを検証してメモリ安全性を保証します",
                ),
            ],
        ),
        (
            "session-python",
            vec![
                ("user-1", "Pythonのasync/awaitについて質問があります"),
                ("agent-1", "Pythonのasyncioは非同期I/Oフレームワークです"),
                ("user-1", "イベントループの仕組みは？"),
                (
                    "agent-1",
                    "asyncioはシングルスレッドのイベントループでコルーチンをスケジューリングします",
                ),
            ],
        ),
        (
            "session-db",
            vec![
                ("user-1", "SQLiteのWALモードの利点を教えてください"),
                (
                    "agent-1",
                    "WALモードは書き込みと読み取りの並行性を向上させます",
                ),
                ("user-1", "パフォーマンスへの影響は？"),
                (
                    "agent-1",
                    "読み取りが書き込みをブロックしないため、高負荷時のスループットが向上します",
                ),
            ],
        ),
    ];

    for (session_id, messages) in &sessions {
        for (i, (speaker, content)) in messages.iter().enumerate() {
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "agent-1".to_string(),
                session_id: session_id.to_string(),
                log_type: "message".to_string(),
                content: content.to_string(),
                speaker_id: Some(speaker.to_string()),
                turn_number: Some(i as i32),
                metadata_json: None,
                created_at: None,
            };
            opencrab_db::queries::insert_session_log(&conn, &log).unwrap();
        }
    }

    // 各セッションごとに適切なタイトル・サマリーを返すMockLlm
    struct IndexMockLlm;

    #[async_trait]
    impl LlmClient for IndexMockLlm {
        async fn chat(&self, req: ChatRequestSimple) -> anyhow::Result<ChatResponseSimple> {
            let content = req
                .messages
                .last()
                .map(|m| m.content.as_str())
                .unwrap_or("");
            let summary = if content.contains("ライフタイム") || content.contains("借用") {
                r#"{"title": "Rustのライフタイムと借用チェッカー", "summary": "Rustのライフタイムアノテーションと借用チェッカーの仕組みについての議論。メモリ安全性の保証方法を解説。"}"#
            } else if content.contains("async") || content.contains("Python") {
                r#"{"title": "Pythonのasync/awaitとイベントループ", "summary": "Pythonのasyncioフレームワークとイベントループの仕組みについての質疑応答。"}"#
            } else if content.contains("SQLite") || content.contains("WAL") {
                r#"{"title": "SQLiteのWALモードとパフォーマンス", "summary": "SQLiteのWALモードの利点とパフォーマンスへの影響についての議論。並行性の向上を解説。"}"#
            } else {
                r#"{"title": "一般的な議論", "summary": "トピックの議論。"}"#
            };
            Ok(ChatResponseSimple {
                content: Some(summary.to_string()),
                tool_calls: vec![],
                finish_reason: "stop".to_string(),
                usage: None,
            })
        }
    }

    let db = Arc::new(Mutex::new(conn));

    // インデックス構築
    opencrab_core::memory_index::IndexBuilder::build_incremental(
        &db,
        "agent-1",
        &IndexMockLlm,
        "test-model",
        50,
    )
    .await
    .unwrap();

    let dir = tempfile::TempDir::new().unwrap();
    let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();

    let ctx = ActionContext {
        caller: CallerIdentity::Owner,
        agent_id: "agent-1".to_string(),
        agent_name: "Test Agent".to_string(),
        session_id: Some("session-1".to_string()),
        db: Arc::clone(&db),
        workspace: Arc::new(ws),
        last_metrics_id: Arc::new(Mutex::new(None)),
        model_override: Arc::new(Mutex::new(None)),
        current_purpose: Arc::new(Mutex::new("conversation".to_string())),
        runtime_info: Arc::new(Mutex::new(opencrab_actions::RuntimeInfo {
            default_model: "mock:test-model".to_string(),
            active_model: None,
            available_providers: vec!["mock".to_string()],
            gateway: "test".to_string(),
        })),
    };

    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
    (dir, executor, db)
}

/// Agentic RAG E2E: browse → ツリーから関連ノード選択 → retrieve → 最終回答
///
/// MockLlmが以下の3ステップを模擬:
/// 1. browse_memory_index を呼んでツリーを取得
/// 2. ツリーを見てRust関連のtopicノードIDを選び retrieve_memory_nodes を呼ぶ
/// 3. 取得した全文テキストをもとに最終回答を生成
#[tokio::test]
async fn test_agentic_rag_browse_then_retrieve() {
    let (_dir, executor, db) = setup_with_indexed_memory().await;

    // まずインデックスの状態を確認してtopicノードIDを取得
    let rust_topic_id = {
        let conn = db.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&conn, "agent-1").unwrap();
        let rust_topic = tree
            .iter()
            .find(|n| n.node_type == "topic" && n.title.contains("Rust"))
            .expect("Rust topic node should exist");
        rust_topic.id.clone()
    };

    // MockLlm: browse → (ツリーを見てRustトピックを選択) → retrieve → 最終回答
    let llm = MockLlm::new(vec![
        // Step 1: LLMが browse_memory_index を呼ぶ
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-browse".to_string(),
                name: "browse_memory_index".to_string(),
                arguments: serde_json::json!({"max_depth": 3}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        // Step 2: ツリー結果を見てRustトピックのnode_idで retrieve を呼ぶ
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-retrieve".to_string(),
                name: "retrieve_memory_nodes".to_string(),
                arguments: serde_json::json!({"node_ids": [rust_topic_id]}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        // Step 3: 取得した全文テキストをもとに最終回答
        ChatResponseSimple {
            content: Some(
                "過去の会話によると、Rustのライフタイムは参照の有効期間を示すアノテーションで、借用チェッカーがこれを検証してメモリ安全性を保証します。"
                    .to_string(),
            ),
            tool_calls: vec![],
            finish_reason: "stop".to_string(),
            usage: None,
        },
    ]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    let result = engine
        .run(
            "You are a knowledgeable agent with memory search capabilities.",
            "Rustのライフタイムについて過去に何を議論しましたか？",
            "mock-model",
        )
        .await
        .unwrap();

    // 3イテレーション（browse → retrieve → final text）
    assert_eq!(result.iterations, 3);
    assert_eq!(result.tool_calls_made, 2);
    assert!(!result.stopped_by_limit);
    // 最終回答にライフタイムの内容が含まれている
    assert!(result.response.contains("ライフタイム"));
    assert!(result.response.contains("借用チェッカー"));
}

/// Agentic RAG: browseの結果にノードがない場合でもエラーにならない
#[tokio::test]
async fn test_agentic_rag_empty_index() {
    let (_dir, executor) = setup();

    let llm = MockLlm::new(vec![
        // LLMがbrowseを呼ぶ
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-1".to_string(),
                name: "browse_memory_index".to_string(),
                arguments: serde_json::json!({}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        // 空のツリーを受け取り、FTS検索にフォールバック
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-2".to_string(),
                name: "search_my_history".to_string(),
                arguments: serde_json::json!({"query": "Rust"}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        // 最終回答
        ChatResponseSimple {
            content: Some("記憶インデックスに該当する情報がありませんでした。".to_string()),
            tool_calls: vec![],
            finish_reason: "stop".to_string(),
            usage: None,
        },
    ]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    let result = engine
        .run("system", "何か調べて", "mock-model")
        .await
        .unwrap();

    assert_eq!(result.iterations, 3);
    assert_eq!(result.tool_calls_made, 2);
    assert!(!result.stopped_by_limit);
}

/// Agentic RAG: 複数ノードを同時にretrieveして横断的に回答
#[tokio::test]
async fn test_agentic_rag_multi_node_retrieve() {
    let (_dir, executor, db) = setup_with_indexed_memory().await;

    // Rust と SQLite の2つのtopicノードIDを取得
    let (rust_topic_id, db_topic_id) = {
        let conn = db.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&conn, "agent-1").unwrap();
        let rust = tree
            .iter()
            .find(|n| n.node_type == "topic" && n.title.contains("Rust"))
            .unwrap();
        let db_node = tree
            .iter()
            .find(|n| n.node_type == "topic" && n.title.contains("SQLite"))
            .unwrap();
        (rust.id.clone(), db_node.id.clone())
    };

    let llm = MockLlm::new(vec![
        // browse
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-1".to_string(),
                name: "browse_memory_index".to_string(),
                arguments: serde_json::json!({}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        // 2つのノードを同時にretrieve
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-2".to_string(),
                name: "retrieve_memory_nodes".to_string(),
                arguments: serde_json::json!({"node_ids": [rust_topic_id, db_topic_id]}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        // 横断的な回答
        ChatResponseSimple {
            content: Some(
                "RustとSQLiteの両方について議論しました。Rustではライフタイムと借用チェッカー、SQLiteではWALモードのパフォーマンスについて話しました。"
                    .to_string(),
            ),
            tool_calls: vec![],
            finish_reason: "stop".to_string(),
            usage: None,
        },
    ]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    let result = engine
        .run(
            "You are a knowledgeable agent.",
            "過去にRustとデータベースについてどんな議論をしましたか？",
            "mock-model",
        )
        .await
        .unwrap();

    assert_eq!(result.iterations, 3);
    assert_eq!(result.tool_calls_made, 2);
    // 両方のトピックに言及している
    assert!(result.response.contains("Rust"));
    assert!(result.response.contains("SQLite"));
}

/// Agentic RAG vs FTS: 同じクエリに対しツリー検索とFTS検索の両方を使い分ける
#[tokio::test]
async fn test_agentic_rag_combined_with_fts() {
    let (_dir, executor, db) = setup_with_indexed_memory().await;

    let python_topic_id = {
        let conn = db.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&conn, "agent-1").unwrap();
        let python = tree
            .iter()
            .find(|n| n.node_type == "topic" && n.title.contains("Python"))
            .unwrap();
        python.id.clone()
    };

    let llm = MockLlm::new(vec![
        // Step 1: まずFTS検索でキーワードヒット
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-1".to_string(),
                name: "search_my_history".to_string(),
                arguments: serde_json::json!({"query": "async await", "limit": 5}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        // Step 2: FTS結果を見てより詳しい文脈が欲しい → browse
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-2".to_string(),
                name: "browse_memory_index".to_string(),
                arguments: serde_json::json!({}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        // Step 3: Pythonトピックを特定してretrieve
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-3".to_string(),
                name: "retrieve_memory_nodes".to_string(),
                arguments: serde_json::json!({"node_ids": [python_topic_id]}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        // Step 4: 全情報をもとに最終回答
        ChatResponseSimple {
            content: Some(
                "Pythonのasync/awaitとasyncioイベントループについて過去に議論しました。シングルスレッドのイベントループでコルーチンをスケジューリングする仕組みです。"
                    .to_string(),
            ),
            tool_calls: vec![],
            finish_reason: "stop".to_string(),
            usage: None,
        },
    ]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    let result = engine
        .run(
            "You are a knowledgeable agent with both keyword search and tree-based memory.",
            "非同期プログラミングについて過去に何を話しましたか？",
            "mock-model",
        )
        .await
        .unwrap();

    // 4イテレーション: FTS → browse → retrieve → text
    assert_eq!(result.iterations, 4);
    assert_eq!(result.tool_calls_made, 3);
    assert!(!result.stopped_by_limit);
    assert!(result.response.contains("async/await"));
    assert!(result.response.contains("イベントループ"));
}

/// Unknown action returns error result, engine continues and produces final text.
#[tokio::test]
async fn test_engine_unknown_action_handled() {
    let (_dir, executor) = setup();

    let llm = MockLlm::new(vec![
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-1".to_string(),
                name: "nonexistent_action".to_string(),
                arguments: serde_json::json!({}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        ChatResponseSimple {
            content: Some("That action was not found.".to_string()),
            tool_calls: vec![],
            finish_reason: "stop".to_string(),
            usage: None,
        },
    ]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    let result = engine
        .run("system", "try something", "mock-model")
        .await
        .unwrap();

    assert_eq!(result.tool_calls_made, 1);
    assert!(!result.stopped_by_limit);
}
