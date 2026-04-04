use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

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
    persona_name: String,
    personality: Option<String>,
}

impl DailyLogIndexer {
    pub fn new(db: Arc<Mutex<Connection>>, llm_client: Arc<dyn LlmClient>, model: String, persona_name: String, personality: Option<String>) -> Self {
        Self {
            db,
            llm_client,
            model,
            persona_name,
            personality,
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
            let db = self
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            opencrab_db::queries::get_daily_log_watermark(&db, agent_id)?
                .map(|w| w.last_indexed_date)
                .unwrap_or_else(|| "0000-00-00".to_string())
        };

        let entries = {
            let db = self
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            opencrab_db::queries::get_unindexed_daily_logs(&db, agent_id, &last_indexed_date)?
        };

        if entries.is_empty() {
            return Ok(stats);
        }

        self.ensure_root_node(agent_id, &now)?;

        let root_id = format!("{agent_id}:daily_log:root");
        let mut periods_seen = HashSet::new();
        for entry in &entries {
            match self
                .process_entry(agent_id, entry, &root_id, &now, &mut periods_seen)
                .await
            {
                Ok(llm_calls) => {
                    stats.llm_calls += llm_calls;
                    stats.days_indexed += 1;
                    let db = self
                        .db
                        .lock()
                        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                    opencrab_db::queries::upsert_daily_log_watermark(
                        &db,
                        &opencrab_db::queries::DailyLogWatermarkRow {
                            agent_id: agent_id.to_string(),
                            last_indexed_date: entry.date_str.clone(),
                            updated_at: now.clone(),
                        },
                    )?;
                }
                Err(e) => {
                    let error = e.to_string();
                    if error.to_lowercase().contains("context") {
                        tracing::error!(
                            date=%entry.date_str,
                            content_bytes=entry.content.len(),
                            error=%error,
                            "skipping daily log entry due to context-related error"
                        );
                    } else {
                        tracing::warn!(
                            date=%entry.date_str,
                            error=%error,
                            "skipping daily log entry due to error"
                        );
                    }
                    stats.days_skipped += 1;
                    continue;
                }
            }
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
                let db = self
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                opencrab_db::queries::get_daily_log_by_date(&db, agent_id, date_str)?
            };
            if let Some(entry) = entry {
                let year_month = &date_str[..7];
                let period_id = self.ensure_period_node(agent_id, year_month, &root_id, &now)?;
                let ((day_summary, topics), _) = self
                    .summarize_day_with_retry(date_str, &entry.content)
                    .await?;
                let daily_id = format!("{agent_id}:daily_log:daily:{date_str}");

                {
                    let db = self
                        .db
                        .lock()
                        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                    let daily_short_id = opencrab_db::queries::next_short_id(&db, agent_id, "d")?;
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
                        short_id: Some(daily_short_id),
                    };
                    opencrab_db::queries::upsert_daily_log_index_node(&db, &daily_node)?;
                }

                for (i, topic) in topics.iter().enumerate().take(5) {
                    let topic_id = format!("{agent_id}:daily_log:topic:{date_str}:{i}");
                    let db = self
                        .db
                        .lock()
                        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                    let topic_short_id = opencrab_db::queries::next_short_id(&db, agent_id, "t")?;
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
                        short_id: Some(topic_short_id),
                    };
                    opencrab_db::queries::upsert_daily_log_index_node(&db, &topic_node)?;
                }
            }
        }
        self.update_child_counts(agent_id)?;
        Ok(())
    }

    async fn summarize_day(&self, date: &str, content: &str) -> Result<(String, Vec<TopicJson>)> {
        let prompt = if let Some(p) = self.personality.as_ref().filter(|s| !s.is_empty()) {
            format!(
                "あなたは {} です。\n{}\n\n以下は {} にあなたが体験した1日のログです。\nあなた自身の記憶として、以下の観点を含めて要約してください:\n\n1. 学んだこと・技術知見（新しく知ったこと、理解が深まったこと）\n2. 判断の理由（なぜそうしたか、どういう選択肢があったか）\n3. 関係性・感情（誰と何をしたか、どう感じたか）\n4. 失敗と教訓（うまくいかなかったこと、次回への学び）\n\n一人称で書いてください。客観的なイベントログではなく、あなたの記憶として。\n\nJSONのみで出力 (コードブロック不要):\n{{\"day_summary\":\"50字以内の1行要約\",\"topics\":[{{\"title\":\"20字以内\",\"summary\":\"100字以内\"}}]}}\n\nログ:\n{}",
                self.persona_name, p, date, content
            )
        } else {
            format!(
                "以下は {} にあなたが体験した1日のログです。\n一人称視点で記憶として要約してください。\n\n1. 学んだこと・技術知見\n2. 判断の理由\n3. 関係性・感情\n4. 失敗と教訓\n\nJSONのみで出力 (コードブロック不要):\n{{\"day_summary\":\"50字以内の1行要約\",\"topics\":[{{\"title\":\"20字以内\",\"summary\":\"100字以内\"}}]}}\n\nログ:\n{}",
                date, content
            )
        };
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
            max_tokens: Some(4096),
        };
        let resp = self.llm_client.chat(request).await?;
        let text = resp.content.unwrap_or_default();
        let json_str = text
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        let parsed = serde_json::from_str::<DaySummaryJson>(json_str)?;
        Ok((parsed.day_summary, parsed.topics))
    }

    async fn summarize_day_with_retry(
        &self,
        date: &str,
        content: &str,
    ) -> Result<((String, Vec<TopicJson>), usize)> {
        const NON_RETRYABLE_PATTERNS: [&str; 7] = [
            "400",
            "bad request",
            "invalid_request_error",
            "context",
            "too large",
            "maximum context",
            "context length",
        ];

        let mut attempts = 0;
        loop {
            attempts += 1;
            match self.summarize_day(date, content).await {
                Ok(result) => return Ok((result, attempts)),
                Err(err) => {
                    let err_text = err.to_string();
                    let err_lower = err_text.to_lowercase();
                    let should_skip = NON_RETRYABLE_PATTERNS
                        .iter()
                        .any(|pattern| err_lower.contains(pattern));

                    if should_skip || attempts >= 3 {
                        return Err(err);
                    }

                    let backoff_secs = attempts as u64;
                    tracing::warn!(
                        date,
                        attempt = attempts,
                        backoff_secs,
                        error = %err_text,
                        "daily log summarization failed, retrying"
                    );
                    sleep(Duration::from_secs(backoff_secs)).await;
                }
            }
        }
    }

    async fn process_entry(
        &self,
        agent_id: &str,
        entry: &opencrab_db::queries::DailyLogEntry,
        root_id: &str,
        now: &str,
        periods_seen: &mut HashSet<String>,
    ) -> Result<usize> {
        let date_str = &entry.date_str;
        let year_month = &date_str[..7];
        let period_id = self.ensure_period_node(agent_id, year_month, root_id, now)?;
        periods_seen.insert(year_month.to_string());

        let ((day_summary, topics), llm_calls) = self
            .summarize_day_with_retry(date_str, &entry.content)
            .await?;

        let daily_id = format!("{agent_id}:daily_log:daily:{date_str}");
        {
            let db = self
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            let daily_short_id = opencrab_db::queries::next_short_id(&db, agent_id, "d")?;
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
                created_at: now.to_string(),
                updated_at: now.to_string(),
                short_id: Some(daily_short_id),
            };
            opencrab_db::queries::upsert_daily_log_index_node(&db, &daily_node)?;
        }

        for (i, topic) in topics.iter().enumerate().take(5) {
            let topic_id = format!("{agent_id}:daily_log:topic:{date_str}:{i}");
            let db = self
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            let topic_short_id = opencrab_db::queries::next_short_id(&db, agent_id, "t")?;
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
                created_at: now.to_string(),
                updated_at: now.to_string(),
                short_id: Some(topic_short_id),
            };
            opencrab_db::queries::upsert_daily_log_index_node(&db, &topic_node)?;
        }

        Ok(llm_calls)
    }

    fn ensure_root_node(&self, agent_id: &str, now: &str) -> Result<()> {
        let root_id = format!("{agent_id}:daily_log:root");
        let db = self
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
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
                short_id: None,
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
        let db = self
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
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
                short_id: None,
            };
            opencrab_db::queries::upsert_daily_log_index_node(&db, &period)?;
        }
        Ok(period_id)
    }

    fn update_child_counts(&self, agent_id: &str) -> Result<()> {
        let db = self
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
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
            let db = self
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
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

    struct RecordingMockLlm {
        last_request: Arc<Mutex<Option<ChatRequestSimple>>>,
    }

    #[async_trait]
    impl LlmClient for RecordingMockLlm {
        async fn chat(&self, req: ChatRequestSimple) -> Result<ChatResponseSimple> {
            *self.last_request.lock().unwrap() = Some(req);
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
        let indexer = DailyLogIndexer::new(conn, Arc::new(MockLlm), "test-model".to_string(), String::new(), None);
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
            DailyLogIndexer::new(conn.clone(), Arc::new(MockLlm), "test-model".to_string(), String::new(), None);
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
        let conn = Arc::new(Mutex::new(db));
        let indexer =
            DailyLogIndexer::new(conn.clone(), Arc::new(MockLlm), "test-model".to_string(), String::new(), None);
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
            DailyLogIndexer::new(conn.clone(), Arc::new(MockLlm), "test-model".to_string(), String::new(), None);
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
            DailyLogIndexer::new(conn.clone(), Arc::new(MockLlm), "test-model".to_string(), String::new(), None);
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
        let conn = Arc::new(Mutex::new(db));
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
        let prompt = &request.messages[0].content;
        assert!(prompt.contains(&content));
    }

    /// T-2.3: DailyLogIndexer の要約プロンプトにペルソナ情報が含まれる
    #[tokio::test]
    async fn test_daily_persona_prompt_contains_persona_info() {
        let db = opencrab_db::init_memory().unwrap();
        insert_daily_log(&db, "agent-1", "2026-02-01", "kojiraとRustの設計を議論した");
        let conn = Arc::new(Mutex::new(db));
        let last_request = Arc::new(Mutex::new(None));
        let indexer = DailyLogIndexer::new(
            conn,
            Arc::new(RecordingMockLlm { last_request: last_request.clone() }),
            "test-model".to_string(),
            "のすたろう".to_string(),
            Some("17歳のオタク高校生。クールに振る舞うけど根はオタク。".to_string()),
        );
        let stats = indexer.run("agent-1").await.unwrap();
        assert_eq!(stats.days_indexed, 1);

        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = &request.messages[0].content;
        assert!(prompt.contains("のすたろう"), "プロンプトにpersona_nameが含まれるべき");
        assert!(prompt.contains("17歳のオタク高校生"), "プロンプトにpersonalityが含まれるべき");
        assert!(prompt.contains("学んだこと") || prompt.contains("技術知見"), "技術知見軸");
        assert!(prompt.contains("判断の理由") || prompt.contains("判断"), "判断軸");
        assert!(prompt.contains("関係性") || prompt.contains("感情"), "関係性軸");
        assert!(prompt.contains("失敗") || prompt.contains("教訓"), "失敗・教訓軸");
    }

    /// T-2.4: DailyLogIndexer でペルソナなしでも動作する
    #[tokio::test]
    async fn test_daily_persona_empty_works() {
        let db = opencrab_db::init_memory().unwrap();
        insert_daily_log(&db, "agent-1", "2026-02-01", "テストログ");
        let conn = Arc::new(Mutex::new(db));
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
}
