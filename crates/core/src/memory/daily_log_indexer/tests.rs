use super::*;
use crate::engine::{ChatRequest, ChatResponse, LlmClient};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

struct MockLlm;

#[async_trait]
impl LlmClient for MockLlm {
    async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse::text(r#"{"day_summary":"テスト要約","topics":[{"title":"トピック1","summary":"トピック1の詳細"}]}"#.to_string()))
    }
}

struct RecordingMockLlm {
    last_request: Arc<Mutex<Option<ChatRequest>>>,
}

#[async_trait]
impl LlmClient for RecordingMockLlm {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
        *self.last_request.lock().unwrap() = Some(req);
        Ok(ChatResponse::text(r#"{"day_summary":"テスト要約","topics":[{"title":"トピック1","summary":"トピック1の詳細"}]}"#.to_string()))
    }
}

fn insert_daily_log(conn: &rusqlite::Connection, agent_id: &str, date: &str, content: &str) {
    opencrab_db::queries::upsert_curated_memory(
        conn,
        &opencrab_db::queries::CuratedMemoryRow {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            category: format!("daily_log/{date}"),
            content: content.to_string(),
            created_at: String::new(),
        },
    )
    .unwrap();
}

#[tokio::test]
async fn test_run_empty() {
    let db = opencrab_db::init_memory().unwrap();
    let conn = opencrab_db::Db::from_connection(db);
    let indexer = DailyLogIndexer::new(
        conn,
        Arc::new(MockLlm),
        "test-model".to_string(),
        String::new(),
        None,
    );
    let stats = indexer.run("agent-1").await.unwrap();
    assert_eq!(stats.days_indexed, 0);
}

#[tokio::test]
async fn test_run_indexes_daily_logs() {
    let db = opencrab_db::init_memory().unwrap();
    insert_daily_log(&db, "agent-1", "2026-02-01", "2月1日のログ");
    insert_daily_log(&db, "agent-1", "2026-02-02", "2月2日のログ");
    let conn = opencrab_db::Db::from_connection(db);
    let indexer = DailyLogIndexer::new(
        conn.clone(),
        Arc::new(MockLlm),
        "test-model".to_string(),
        String::new(),
        None,
    );
    let stats = indexer.run("agent-1").await.unwrap();
    assert_eq!(stats.days_indexed, 2);
    assert_eq!(stats.periods_updated, 1);

    let db = conn.lock().unwrap();
    let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
    let daily_log_nodes: Vec<_> = tree
        .iter()
        .filter(|n| n.source_type == "daily_log")
        .collect();
    assert!(daily_log_nodes.len() >= 5, "root+period+2daily+2topic >= 5");
    assert!(daily_log_nodes.iter().any(|n| n.node_type == "root"));
    assert!(daily_log_nodes.iter().any(|n| n.node_type == "period"));
    assert_eq!(
        daily_log_nodes
            .iter()
            .filter(|n| n.node_type == "daily")
            .count(),
        2
    );
}

#[tokio::test]
async fn test_run_idempotent() {
    let db = opencrab_db::init_memory().unwrap();
    insert_daily_log(&db, "agent-1", "2026-02-01", "ログ内容");
    let conn = opencrab_db::Db::from_connection(db);
    let indexer = DailyLogIndexer::new(
        conn.clone(),
        Arc::new(MockLlm),
        "test-model".to_string(),
        String::new(),
        None,
    );
    let r1 = indexer.run("agent-1").await.unwrap();
    assert_eq!(r1.days_indexed, 1);
    let r2 = indexer.run("agent-1").await.unwrap();
    assert_eq!(r2.days_indexed, 0);
}

#[tokio::test]
async fn test_rebuild_reindexes_all() {
    let db = opencrab_db::init_memory().unwrap();
    insert_daily_log(&db, "agent-1", "2026-02-01", "ログ内容");
    let conn = opencrab_db::Db::from_connection(db);
    let indexer = DailyLogIndexer::new(
        conn.clone(),
        Arc::new(MockLlm),
        "test-model".to_string(),
        String::new(),
        None,
    );
    indexer.run("agent-1").await.unwrap();
    let stats = indexer.rebuild("agent-1").await.unwrap();
    assert_eq!(stats.days_indexed, 1);
}

#[tokio::test]
async fn test_agent_isolation() {
    let db = opencrab_db::init_memory().unwrap();
    insert_daily_log(&db, "agent-1", "2026-02-01", "エージェント1のログ");
    insert_daily_log(&db, "agent-2", "2026-02-01", "エージェント2のログ");
    let conn = opencrab_db::Db::from_connection(db);
    let indexer = DailyLogIndexer::new(
        conn.clone(),
        Arc::new(MockLlm),
        "test-model".to_string(),
        String::new(),
        None,
    );
    indexer.run("agent-1").await.unwrap();
    indexer.run("agent-2").await.unwrap();

    let db = conn.lock().unwrap();
    let tree1 = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
    let tree2 = opencrab_db::queries::get_index_tree(&db, "agent-2").unwrap();
    let dl1: Vec<_> = tree1
        .iter()
        .filter(|n| n.source_type == "daily_log")
        .collect();
    let dl2: Vec<_> = tree2
        .iter()
        .filter(|n| n.source_type == "daily_log")
        .collect();
    assert!(!dl1.is_empty());
    assert!(!dl2.is_empty());
    for n1 in &dl1 {
        assert!(dl2.iter().all(|n2| n2.id != n1.id));
    }
}

#[tokio::test]
async fn test_large_content_no_truncation() {
    let db = opencrab_db::init_memory().unwrap();
    let content = "あ".repeat(1500);
    assert!(content.len() > 4096);
    insert_daily_log(&db, "agent-1", "2026-02-03", &content);
    let conn = opencrab_db::Db::from_connection(db);
    let last_request = Arc::new(Mutex::new(None));
    let indexer = DailyLogIndexer::new(
        conn,
        Arc::new(RecordingMockLlm {
            last_request: last_request.clone(),
        }),
        "test-model".to_string(),
        String::new(),
        None,
    );

    let stats = indexer.run("agent-1").await.unwrap();

    assert_eq!(stats.days_indexed, 1);
    assert_eq!(stats.days_skipped, 0);

    let request = last_request.lock().unwrap().clone().unwrap();
    let prompt = request.messages[0].text_content().unwrap_or("");
    assert!(prompt.contains(&content));
}

/// T-2.3: DailyLogIndexer の要約プロンプトにペルソナ情報が含まれる
#[tokio::test]
async fn test_daily_persona_prompt_contains_persona_info() {
    let db = opencrab_db::init_memory().unwrap();
    insert_daily_log(&db, "agent-1", "2026-02-01", "ownerとRustの設計を議論した");
    let conn = opencrab_db::Db::from_connection(db);
    let last_request = Arc::new(Mutex::new(None));
    let indexer = DailyLogIndexer::new(
        conn,
        Arc::new(RecordingMockLlm {
            last_request: last_request.clone(),
        }),
        "test-model".to_string(),
        "エージェントC".to_string(),
        Some("17歳のオタク高校生。クールに振る舞うけど根はオタク。".to_string()),
    );
    let stats = indexer.run("agent-1").await.unwrap();
    assert_eq!(stats.days_indexed, 1);

    let request = last_request.lock().unwrap().clone().unwrap();
    let prompt = request.messages[0].text_content().unwrap_or("");
    assert!(
        prompt.contains("エージェントC"),
        "プロンプトにpersona_nameが含まれるべき"
    );
    assert!(
        prompt.contains("17歳のオタク高校生"),
        "プロンプトにpersonalityが含まれるべき"
    );
    assert!(
        prompt.contains("学んだこと") || prompt.contains("技術知見"),
        "技術知見軸"
    );
    assert!(
        prompt.contains("判断の理由") || prompt.contains("判断"),
        "判断軸"
    );
    assert!(
        prompt.contains("関係性") || prompt.contains("感情"),
        "関係性軸"
    );
    assert!(
        prompt.contains("失敗") || prompt.contains("教訓"),
        "失敗・教訓軸"
    );
}

/// T-2.4: DailyLogIndexer でペルソナなしでも動作する
#[tokio::test]
async fn test_daily_persona_empty_works() {
    let db = opencrab_db::init_memory().unwrap();
    insert_daily_log(&db, "agent-1", "2026-02-01", "テストログ");
    let conn = opencrab_db::Db::from_connection(db);
    let indexer = DailyLogIndexer::new(
        conn.clone(),
        Arc::new(MockLlm),
        "test-model".to_string(),
        String::new(),
        None,
    );
    let stats = indexer.run("agent-1").await.unwrap();
    assert_eq!(stats.days_indexed, 1);
}
