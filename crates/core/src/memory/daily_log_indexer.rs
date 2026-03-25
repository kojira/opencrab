use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::engine::{ChatMessage, ChatRequestSimple, LlmClient};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyLogIndexStats {
    pub days_indexed: usize,
    pub days_skipped: usize,
    pub periods_updated: usize,
    pub llm_calls: usize,
}

#[derive(Debug, Deserialize)]
struct DaySummaryJson {
    day_summary: String,
    topics: Vec<TopicJson>,
}

#[derive(Debug, Deserialize)]
struct TopicJson {
    title: String,
    summary: String,
}

pub struct DailyLogIndexer {
    db: Arc<Mutex<Connection>>,
    llm_client: Arc<dyn LlmClient>,
    model: String,
}

impl DailyLogIndexer {
    pub fn new(db: Arc<Mutex<Connection>>, llm_client: Arc<dyn LlmClient>, model: String) -> Self {
        Self {
            db,
            llm_client,
            model,
        }
    }

    pub async fn run(&self, agent_id: &str) -> Result<DailyLogIndexStats> {
        let now = Utc::now().to_rfc3339();
        let mut stats = DailyLogIndexStats {
            days_indexed: 0,
            days_skipped: 0,
            periods_updated: 0,
            llm_calls: 0,
        };

        let last_indexed_date = {
            let db = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            opencrab_db::queries::get_daily_log_watermark(&db, agent_id)?
                .map(|w| w.last_indexed_date)
                .unwrap_or_else(|| "0000-00-00".to_string())
        };

        let entries = {
            let db = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            opencrab_db::queries::get_unindexed_daily_logs(&db, agent_id, &last_indexed_date)?
        };

        if entries.is_empty() {
            return Ok(stats);
        }

        self.ensure_root_node(agent_id, &now)?;

        let root_id = format!("{agent_id}:daily_log:root");
        let mut periods_seen = HashSet::new();
        let mut last_processed_date = last_indexed_date.clone();

        for entry in &entries {
            let date_str = &entry.date_str;
            let year_month = &date_str[..7];
            let period_id = self.ensure_period_node(agent_id, year_month, &root_id, &now)?;
            periods_seen.insert(year_month.to_string());

            let (day_summary, topics) = self.summarize_day(date_str, &entry.content).await;
            stats.llm_calls += 1;

            let daily_id = format!("{agent_id}:daily_log:daily:{date_str}");
            {
                let db = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                let daily_node = opencrab_db::queries::IndexNodeRow {
                    id: daily_id.clone(),
                    agent_id: agent_id.to_string(),
                    parent_id: Some(period_id.clone()),
                    node_type: "daily".to_string(),
                    source_type: "daily_log".to_string(),
                    title: date_str.clone(),
                    summary: day_summary,
                    start_log_id: None,
                    end_log_id: None,
                    source_session_id: None,
                    date_from: Some(date_str.clone()),
                    date_to: Some(date_str.clone()),
                    depth: 2,
                    child_count: 0,
                    token_count: (entry.content.len() / 3) as i32,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                };
                opencrab_db::queries::upsert_daily_log_index_node(&db, &daily_node)?;
            }

            for (i, topic) in topics.iter().enumerate().take(5) {
                let topic_id = format!("{agent_id}:daily_log:topic:{date_str}:{i}");
                let db = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                let topic_node = opencrab_db::queries::IndexNodeRow {
                    id: topic_id,
                    agent_id: agent_id.to_string(),
                    parent_id: Some(daily_id.clone()),
                    node_type: "topic".to_string(),
                    source_type: "daily_log".to_string(),
                    title: topic.title.clone(),
                    summary: topic.summary.clone(),
                    start_log_id: None,
                    end_log_id: None,
                    source_session_id: None,
                    date_from: Some(date_str.clone()),
                    date_to: Some(date_str.clone()),
                    depth: 3,
                    child_count: 0,
                    token_count: 0,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                };
                opencrab_db::queries::upsert_daily_log_index_node(&db, &topic_node)?;
            }

            last_processed_date = date_str.clone();
            stats.days_indexed += 1;
        }

        {
            let db = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            opencrab_db::queries::upsert_daily_log_watermark(
                &db,
                &opencrab_db::queries::DailyLogWatermarkRow {
                    agent_id: agent_id.to_string(),
                    last_indexed_date: last_processed_date,
                    updated_at: now.clone(),
                },
            )?;
        }

        self.update_child_counts(agent_id)?;
        stats.periods_updated = periods_seen.len();
        Ok(stats)
    }

    pub async fn reindex_dates(&self, agent_id: &str, dates: &[String]) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.ensure_root_node(agent_id, &now)?;
        let root_id = format!("{agent_id}:daily_log:root");

        for date_str in dates {
            let entry = {
                let db = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                opencrab_db::queries::get_daily_log_by_date(&db, agent_id, date_str)?
            };
            if let Some(entry) = entry {
                let year_month = &date_str[..7];
                let period_id = self.ensure_period_node(agent_id, year_month, &root_id, &now)?;
                let (day_summary, topics) = self.summarize_day(date_str, &entry.content).await;
                let daily_id = format!("{agent_id}:daily_log:daily:{date_str}");

                {
                    let db = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                    let daily_node = opencrab_db::queries::IndexNodeRow {
                        id: daily_id.clone(),
                        agent_id: agent_id.to_string(),
                        parent_id: Some(period_id),
                        node_type: "daily".to_string(),
                        source_type: "daily_log".to_string(),
                        title: date_str.clone(),
                        summary: day_summary,
                        start_log_id: None,
                        end_log_id: None,
                        source_session_id: None,
                        date_from: Some(date_str.clone()),
                        date_to: Some(date_str.clone()),
                        depth: 2,
                        child_count: 0,
                        token_count: (entry.content.len() / 3) as i32,
                        created_at: now.clone(),
                        updated_at: now.clone(),
                    };
                    opencrab_db::queries::upsert_daily_log_index_node(&db, &daily_node)?;
                }

                for (i, topic) in topics.iter().enumerate().take(5) {
                    let topic_id = format!("{agent_id}:daily_log:topic:{date_str}:{i}");
                    let db = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                    let topic_node = opencrab_db::queries::IndexNodeRow {
                        id: topic_id,
                        agent_id: agent_id.to_string(),
                        parent_id: Some(daily_id.clone()),
                        node_type: "topic".to_string(),
                        source_type: "daily_log".to_string(),
                        title: topic.title.clone(),
                        summary: topic.summary.clone(),
                        start_log_id: None,
                        end_log_id: None,
                        source_session_id: None,
                        date_from: Some(date_str.clone()),
                        date_to: Some(date_str.clone()),
                        depth: 3,
                        child_count: 0,
                        token_count: 0,
                        created_at: now.clone(),
                        updated_at: now.clone(),
                    };
                    opencrab_db::queries::upsert_daily_log_index_node(&db, &topic_node)?;
                }
            }
        }
        self.update_child_counts(agent_id)?;
        Ok(())
    }

    async fn summarize_day(&self, date: &str, content: &str) -> (String, Vec<TopicJson>) {
        let truncated = if content.len() > 4096 {
            &content[..4096]
        } else {
            content
        };
        let prompt = format!(
            "以下は {} の日次ログです。\nJSONのみで出力 (コードブロック不要):\n{{\"day_summary\":\"50字以内の1行要約\",\"topics\":[{{\"title\":\"20字以内\",\"summary\":\"100字以内\"}}]}}\n\nログ:\n{}",
            date, truncated
        );
        let request = ChatRequestSimple {
            model: self.model.clone(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: prompt,
                tool_call_id: None,
                tool_calls: vec![],
                content_parts: vec![],
                cache_control: None,
            }],
            tools: vec![],
            temperature: Some(0.0),
            max_tokens: Some(500),
        };
        match self.llm_client.chat(request).await {
            Ok(resp) => {
                let text = resp.content.unwrap_or_default();
                let json_str = text
                    .trim()
                    .trim_start_matches("```json")
                    .trim_start_matches("```")
                    .trim_end_matches("```")
                    .trim();
                match serde_json::from_str::<DaySummaryJson>(json_str) {
                    Ok(parsed) => (parsed.day_summary, parsed.topics),
                    Err(_) => (
                        format!("{date} の日次ログ"),
                        vec![TopicJson {
                            title: date.to_string(),
                            summary: "要約生成失敗".to_string(),
                        }],
                    ),
                }
            }
            Err(e) => {
                tracing::warn!(date=%date, error=%e, "daily_log LLM summary failed");
                (
                    format!("{date} の日次ログ"),
                    vec![TopicJson {
                        title: date.to_string(),
                        summary: "LLMエラー".to_string(),
                    }],
                )
            }
        }
    }

    fn ensure_root_node(&self, agent_id: &str, now: &str) -> Result<()> {
        let root_id = format!("{agent_id}:daily_log:root");
        let db = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        if opencrab_db::queries::get_index_node(&db, &root_id)?.is_none() {
            let root = opencrab_db::queries::IndexNodeRow {
                id: root_id,
                agent_id: agent_id.to_string(),
                parent_id: None,
                node_type: "root".to_string(),
                source_type: "daily_log".to_string(),
                title: "日次ログ アーカイブ".to_string(),
                summary: "OpenClawワークスペースの日次ログ".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: now.to_string(),
                updated_at: now.to_string(),
            };
            opencrab_db::queries::upsert_daily_log_index_node(&db, &root)?;
        }
        Ok(())
    }

    fn ensure_period_node(
        &self,
        agent_id: &str,
        year_month: &str,
        root_id: &str,
        now: &str,
    ) -> Result<String> {
        let period_id = format!("{agent_id}:daily_log:period:{year_month}");
        let db = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        if opencrab_db::queries::get_index_node(&db, &period_id)?.is_none() {
            let year = &year_month[..4];
            let month = &year_month[5..7];
            let period = opencrab_db::queries::IndexNodeRow {
                id: period_id.clone(),
                agent_id: agent_id.to_string(),
                parent_id: Some(root_id.to_string()),
                node_type: "period".to_string(),
                source_type: "daily_log".to_string(),
                title: format!("{year}年{month}月"),
                summary: format!("{year_month} の日次ログ"),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: Some(format!("{year_month}-01")),
                date_to: None,
                depth: 1,
                child_count: 0,
                token_count: 0,
                created_at: now.to_string(),
                updated_at: now.to_string(),
            };
            opencrab_db::queries::upsert_daily_log_index_node(&db, &period)?;
        }
        Ok(period_id)
    }

    fn update_child_counts(&self, agent_id: &str) -> Result<()> {
        let db = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db.prepare(
            "SELECT id, parent_id FROM memory_index_nodes WHERE agent_id=?1 AND source_type='daily_log'",
        )?;
        let nodes: Vec<(String, Option<String>)> = stmt
            .query_map(rusqlite::params![agent_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        let mut child_counts: HashMap<String, i32> = HashMap::new();
        for (_, parent_id) in &nodes {
            if let Some(pid) = parent_id {
                *child_counts.entry(pid.clone()).or_default() += 1;
            }
        }

        let now = Utc::now().to_rfc3339();
        db.execute(
            "UPDATE memory_index_nodes
             SET child_count=0, updated_at=?2
             WHERE agent_id=?1 AND source_type='daily_log'",
            rusqlite::params![agent_id, now],
        )?;
        for (node_id, count) in &child_counts {
            db.execute(
                "UPDATE memory_index_nodes SET child_count=?1, updated_at=?2 WHERE id=?3",
                rusqlite::params![count, now, node_id],
            )?;
        }
        Ok(())
    }

    pub async fn rebuild(&self, agent_id: &str) -> Result<DailyLogIndexStats> {
        {
            let db = self.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            db.execute(
                "DELETE FROM memory_index_nodes WHERE agent_id=?1 AND source_type='daily_log'",
                rusqlite::params![agent_id],
            )?;
            opencrab_db::queries::delete_daily_log_watermark(&db, agent_id)?;
        }
        self.run(agent_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{ChatRequestSimple, ChatResponseSimple, LlmClient};
    use async_trait::async_trait;

    struct MockLlm;

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn chat(&self, _req: ChatRequestSimple) -> Result<ChatResponseSimple> {
            Ok(ChatResponseSimple {
                content: Some(r#"{"day_summary":"テスト要約","topics":[{"title":"トピック1","summary":"トピック1の詳細"}]}"#.to_string()),
                tool_calls: vec![],
                finish_reason: "stop".to_string(),
                usage: None,
            })
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
        let conn = Arc::new(Mutex::new(db));
        let indexer = DailyLogIndexer::new(conn, Arc::new(MockLlm), "test-model".to_string());
        let stats = indexer.run("agent-1").await.unwrap();
        assert_eq!(stats.days_indexed, 0);
    }

    #[tokio::test]
    async fn test_run_indexes_daily_logs() {
        let db = opencrab_db::init_memory().unwrap();
        insert_daily_log(&db, "agent-1", "2026-02-01", "2月1日のログ");
        insert_daily_log(&db, "agent-1", "2026-02-02", "2月2日のログ");
        let conn = Arc::new(Mutex::new(db));
        let indexer =
            DailyLogIndexer::new(conn.clone(), Arc::new(MockLlm), "test-model".to_string());
        let stats = indexer.run("agent-1").await.unwrap();
        assert_eq!(stats.days_indexed, 2);
        assert_eq!(stats.periods_updated, 1);

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        let daily_log_nodes: Vec<_> = tree.iter().filter(|n| n.source_type == "daily_log").collect();
        assert!(daily_log_nodes.len() >= 5, "root+period+2daily+2topic >= 5");
        assert!(daily_log_nodes.iter().any(|n| n.node_type == "root"));
        assert!(daily_log_nodes.iter().any(|n| n.node_type == "period"));
        assert_eq!(daily_log_nodes.iter().filter(|n| n.node_type == "daily").count(), 2);
    }

    #[tokio::test]
    async fn test_run_idempotent() {
        let db = opencrab_db::init_memory().unwrap();
        insert_daily_log(&db, "agent-1", "2026-02-01", "ログ内容");
        let conn = Arc::new(Mutex::new(db));
        let indexer =
            DailyLogIndexer::new(conn.clone(), Arc::new(MockLlm), "test-model".to_string());
        let r1 = indexer.run("agent-1").await.unwrap();
        assert_eq!(r1.days_indexed, 1);
        let r2 = indexer.run("agent-1").await.unwrap();
        assert_eq!(r2.days_indexed, 0);
    }

    #[tokio::test]
    async fn test_rebuild_reindexes_all() {
        let db = opencrab_db::init_memory().unwrap();
        insert_daily_log(&db, "agent-1", "2026-02-01", "ログ内容");
        let conn = Arc::new(Mutex::new(db));
        let indexer =
            DailyLogIndexer::new(conn.clone(), Arc::new(MockLlm), "test-model".to_string());
        indexer.run("agent-1").await.unwrap();
        let stats = indexer.rebuild("agent-1").await.unwrap();
        assert_eq!(stats.days_indexed, 1);
    }

    #[tokio::test]
    async fn test_agent_isolation() {
        let db = opencrab_db::init_memory().unwrap();
        insert_daily_log(&db, "agent-1", "2026-02-01", "エージェント1のログ");
        insert_daily_log(&db, "agent-2", "2026-02-01", "エージェント2のログ");
        let conn = Arc::new(Mutex::new(db));
        let indexer =
            DailyLogIndexer::new(conn.clone(), Arc::new(MockLlm), "test-model".to_string());
        indexer.run("agent-1").await.unwrap();
        indexer.run("agent-2").await.unwrap();

        let db = conn.lock().unwrap();
        let tree1 = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        let tree2 = opencrab_db::queries::get_index_tree(&db, "agent-2").unwrap();
        let dl1: Vec<_> = tree1.iter().filter(|n| n.source_type == "daily_log").collect();
        let dl2: Vec<_> = tree2.iter().filter(|n| n.source_type == "daily_log").collect();
        assert!(!dl1.is_empty());
        assert!(!dl2.is_empty());
        for n1 in &dl1 {
            assert!(dl2.iter().all(|n2| n2.id != n1.id));
        }
    }
}
