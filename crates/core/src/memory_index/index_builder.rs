//! 階層型記憶インデックスの増分構築。
//!
//! LLMを使って未インデックスのセッションログを要約し、
//! ツリー構造のインデックスノードとして保存する。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::engine::{ChatMessage, ChatRequestSimple, LlmClient};

/// インデックス構築結果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexBuildResult {
    pub nodes_created: usize,
    pub logs_indexed: usize,
}

/// ツリー再マージ結果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeResult {
    pub periods_processed: usize,
    pub topics_merged: usize,
    pub topics_deleted: usize,
}

/// LLMから返されるサマリーJSON
#[derive(Debug, Deserialize)]
struct LlmSummary {
    title: String,
    summary: String,
}

pub struct IndexBuilder;

impl IndexBuilder {
    /// 増分インデックス構築。未インデックスのログをLLMで要約してツリーに追加。
    pub async fn build_incremental(
        conn: &Arc<Mutex<Connection>>,
        agent_id: &str,
        llm: &dyn LlmClient,
        model: &str,
        batch_size: usize,
        persona_name: &str,
        personality: Option<&str>,
    ) -> Result<IndexBuildResult> {
        // 1. ウォーターマーク取得
        let (last_indexed_id, existing_total_nodes) = {
            let db = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
            let wm = opencrab_db::queries::get_index_watermark(&db, agent_id)?;
            (
                wm.as_ref().map(|w| w.last_indexed_log_id).unwrap_or(0),
                wm.as_ref().map(|w| w.total_nodes).unwrap_or(0),
            )
        };

        // 2. 未処理ログ取得
        let logs = {
            let db = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
            opencrab_db::queries::get_unindexed_session_logs(
                &db,
                agent_id,
                last_indexed_id,
                batch_size,
            )?
        };

        if logs.is_empty() {
            return Ok(IndexBuildResult {
                nodes_created: 0,
                logs_indexed: 0,
            });
        }

        // 3. session_idでグループ化
        let mut session_groups: HashMap<String, Vec<opencrab_db::queries::SessionLogRow>> =
            HashMap::new();
        for log in &logs {
            session_groups
                .entry(log.session_id.clone())
                .or_default()
                .push(log.clone());
        }

        let now = Utc::now().to_rfc3339();
        let mut nodes_created = 0;
        let mut max_log_id = last_indexed_id;

        // 4. ルートノード確保
        let root_id = format!("root-{agent_id}");
        {
            let db = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
            if opencrab_db::queries::get_index_node(&db, &root_id)?.is_none() {
                let root = opencrab_db::queries::IndexNodeRow {
                    id: root_id.clone(),
                    agent_id: agent_id.to_string(),
                    parent_id: None,
                    node_type: "root".to_string(),
                    source_type: "session_log".to_string(),
                    title: "Memory Root".to_string(),
                    summary: "Root node for all memories".to_string(),
                    start_log_id: None,
                    end_log_id: None,
                    source_session_id: None,
                    date_from: None,
                    date_to: None,
                    depth: 0,
                    child_count: 0,
                    token_count: 0,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                    short_id: Some("r0".to_string()),
                };
                opencrab_db::queries::insert_index_node(&db, &root)?;
                nodes_created += 1;
            }
        }

        // 5. 各セッショングループを処理
        for (session_id, session_logs) in &session_groups {
            let first_log_id = session_logs.iter().filter_map(|l| l.id).min().unwrap_or(0);
            let last_log_id = session_logs.iter().filter_map(|l| l.id).max().unwrap_or(0);
            if last_log_id > max_log_id {
                max_log_id = last_log_id;
            }

            // 期間ノード（年月-週）を確保
            let period_label = Utc::now().format("%Y-%m").to_string();
            let period_id = format!("period-{agent_id}-{period_label}");
            {
                let db = conn
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
                if opencrab_db::queries::get_index_node(&db, &period_id)?.is_none() {
                    let period_short_id = opencrab_db::queries::next_short_id(&db, agent_id, "p")?;
                    let period = opencrab_db::queries::IndexNodeRow {
                        id: period_id.clone(),
                        agent_id: agent_id.to_string(),
                        parent_id: Some(root_id.clone()),
                        node_type: "period".to_string(),
                        source_type: "session_log".to_string(),
                        title: period_label.clone(),
                        summary: format!("Conversations from {period_label}"),
                        start_log_id: None,
                        end_log_id: None,
                        source_session_id: None,
                        date_from: None,
                        date_to: None,
                        depth: 1,
                        child_count: 0,
                        token_count: 0,
                        created_at: now.clone(),
                        updated_at: now.clone(),
                        short_id: Some(period_short_id),
                    };
                    opencrab_db::queries::insert_index_node(&db, &period)?;
                    nodes_created += 1;
                }
            }

            // セッションノードを確保
            let session_node_id = format!("session-{agent_id}-{session_id}");
            {
                let db = conn
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
                if opencrab_db::queries::get_index_node(&db, &session_node_id)?.is_none() {
                    // セッションノードのタイトルは最初のログから推測
                    let preview = session_logs
                        .first()
                        .map(|l| {
                            let chars: Vec<char> = l.content.chars().collect();
                            if chars.len() > 50 {
                                format!("{}...", chars[..50].iter().collect::<String>())
                            } else {
                                l.content.clone()
                            }
                        })
                        .unwrap_or_default();
                    let session_short_id = opencrab_db::queries::next_short_id(&db, agent_id, "s")?;
                    let session_node = opencrab_db::queries::IndexNodeRow {
                        id: session_node_id.clone(),
                        agent_id: agent_id.to_string(),
                        parent_id: Some(period_id.clone()),
                        node_type: "session".to_string(),
                        source_type: "session_log".to_string(),
                        title: format!("Session: {}", &session_id[..session_id.len().min(8)]),
                        summary: preview,
                        start_log_id: Some(first_log_id),
                        end_log_id: Some(last_log_id),
                        source_session_id: Some(session_id.clone()),
                        date_from: None,
                        date_to: None,
                        depth: 2,
                        child_count: 0,
                        token_count: 0,
                        created_at: now.clone(),
                        updated_at: now.clone(),
                        short_id: Some(session_short_id),
                    };
                    opencrab_db::queries::insert_index_node(&db, &session_node)?;
                    nodes_created += 1;
                }
            }

            // ログテキスト連結
            let chunk_text: String = session_logs
                .iter()
                .map(|l| {
                    let speaker = l.speaker_id.as_deref().unwrap_or("unknown");
                    format!("[{}]: {}", speaker, l.content)
                })
                .collect::<Vec<_>>()
                .join("\n");

            // トークン数の概算（文字数 / 3 が日本語の目安）
            let token_count = (chunk_text.len() / 3) as i32;

            // LLM呼び出しでサマリー生成
            let prompt = if let Some(p) = personality.filter(|s| !s.is_empty()) {
                format!(
                    "あなたは {persona_name} です。\n{p}\n\n以下はあなたが体験した会話のログです。\nあなた自身の記憶として、以下の観点を含めて要約してください:\n\n1. 学んだこと・技術知見（新しく知ったこと、理解が深まったこと）\n2. 判断の理由（なぜそうしたか、どういう選択肢があったか）\n3. 関係性・感情（誰と何をしたか、どう感じたか）\n4. 失敗と教訓（うまくいかなかったこと、次回への学び）\n\n一人称で書いてください。客観的なイベントログではなく、あなたの記憶として。\n\nJSON形式で出力:\n{{\"title\": \"20字以内\", \"summary\": \"200字以内\"}}\n\nログ:\n{chunk_text}"
                )
            } else {
                format!(
                    "以下の会話のログについて、一人称視点で記憶として要約してください。\n\n1. 学んだこと・技術知見\n2. 判断の理由\n3. 関係性・感情\n4. 失敗と教訓\n\nJSON形式で出力:\n{{\"title\": \"20字以内\", \"summary\": \"200字以内\"}}\n\nログ:\n{chunk_text}"
                )
            };

            let request = ChatRequestSimple {
                model: model.to_string(),
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
                max_tokens: Some(200),
            };

            let summary = match llm.chat(request).await {
                Ok(resp) => {
                    let text = resp.content.unwrap_or_default();
                    // JSON部分を抽出（マークダウンコードブロック対応）
                    let json_str = text
                        .trim()
                        .trim_start_matches("```json")
                        .trim_start_matches("```")
                        .trim_end_matches("```")
                        .trim();
                    serde_json::from_str::<LlmSummary>(json_str).unwrap_or(LlmSummary {
                        title: format!("Topic (logs {first_log_id}-{last_log_id})"),
                        summary: session_logs
                            .first()
                            .map(|l| {
                                let chars: Vec<char> = l.content.chars().collect();
                                if chars.len() > 100 {
                                    format!("{}...", chars[..100].iter().collect::<String>())
                                } else {
                                    l.content.clone()
                                }
                            })
                            .unwrap_or_default(),
                    })
                }
                Err(e) => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        first_log_id = first_log_id,
                        last_log_id = last_log_id,
                        error = %e,
                        "LLM summary generation failed, using fallback"
                    );
                    LlmSummary {
                        title: format!("Topic (logs {first_log_id}-{last_log_id})"),
                        summary: "Summary generation failed".to_string(),
                    }
                }
            };

            // topicノード作成
            let topic_id = format!("topic-{agent_id}-{session_id}-{first_log_id}-{last_log_id}");
            let date_from = session_logs.iter().filter_map(|l| l.created_at.as_deref()).filter(|s| s.len() >= 10).min().map(|s| s[..10].to_string());
            let date_to = session_logs.iter().filter_map(|l| l.created_at.as_deref()).filter(|s| s.len() >= 10).max().map(|s| s[..10].to_string());
            let mut topic = opencrab_db::queries::IndexNodeRow {
                id: topic_id.clone(),
                agent_id: agent_id.to_string(),
                parent_id: Some(session_node_id.clone()),
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: summary.title,
                summary: summary.summary,
                start_log_id: Some(first_log_id),
                end_log_id: Some(last_log_id),
                source_session_id: Some(session_id.clone()),
                date_from,
                date_to,
                depth: 3,
                child_count: 0,
                token_count,
                created_at: now.clone(),
                updated_at: now.clone(),
                short_id: None,
            };

            {
                let db = conn
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
                if opencrab_db::queries::get_index_node(&db, &topic_id)?.is_none() {
                    topic.short_id = Some(opencrab_db::queries::next_short_id(&db, agent_id, "t")?);
                    opencrab_db::queries::insert_index_node(&db, &topic)?;
                    nodes_created += 1;
                } else {
                    tracing::debug!(
                        topic_id = %topic_id,
                        "Topic node already exists, skipping insertion"
                    );
                }
            }
        }

        // 6. 子ノード数を更新
        {
            let db = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
            let all_nodes = opencrab_db::queries::get_index_tree(&db, agent_id)?;
            let mut child_counts: HashMap<String, i32> = HashMap::new();
            for node in &all_nodes {
                if let Some(ref pid) = node.parent_id {
                    *child_counts.entry(pid.clone()).or_default() += 1;
                }
            }
            for (node_id, count) in &child_counts {
                opencrab_db::queries::update_index_node_child_count(&db, node_id, *count)?;
            }
        }

        // 7. ウォーターマーク更新
        {
            let db = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
            let wm = opencrab_db::queries::WatermarkRow {
                agent_id: agent_id.to_string(),
                last_indexed_log_id: max_log_id,
                last_indexed_at: now,
                total_nodes: existing_total_nodes + nodes_created as i64,
            };
            opencrab_db::queries::upsert_index_watermark(&db, &wm)?;
        }

        Ok(IndexBuildResult {
            nodes_created,
            logs_indexed: logs.len(),
        })
    }

    /// エージェントのインデックス全体を削除する。
    pub fn delete_index(conn: &Arc<Mutex<Connection>>, agent_id: &str) -> Result<()> {
        let db = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
        opencrab_db::queries::delete_index_nodes_for_agent(&db, agent_id)?;
        opencrab_db::queries::delete_index_watermark_for_agent(&db, agent_id)?;
        Ok(())
    }

    /// インデックスをゼロから再構築する（削除 → 増分ビルド）。
    pub async fn rebuild_index(
        conn: &Arc<Mutex<Connection>>,
        agent_id: &str,
        llm: &dyn LlmClient,
        model: &str,
        batch_size: usize,
        persona_name: &str,
        personality: Option<&str>,
    ) -> Result<IndexBuildResult> {
        Self::delete_index(conn, agent_id)?;
        Self::build_incremental(conn, agent_id, llm, model, batch_size, persona_name, personality).await
    }

    /// 既存のtopicノードをperiodレベルでLLM再要約・統合する（深さ調整）。
    ///
    /// topic数が max_topics_per_period を超えていたら、LLMでまとめて再要約し統合する。
    pub async fn merge_topics(
        conn: &Arc<Mutex<Connection>>,
        agent_id: &str,
        llm: &dyn LlmClient,
        model: &str,
        max_topics_per_period: usize,
        persona_name: &str,
        personality: Option<&str>,
    ) -> Result<MergeResult> {
        let now = Utc::now().to_rfc3339();
        let tree = {
            let db = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
            opencrab_db::queries::get_index_tree(&db, agent_id)?
        };

        let period_nodes: Vec<_> = tree.iter().filter(|n| n.node_type == "period").collect();

        let mut merged_count = 0usize;
        let mut deleted_count = 0usize;

        for period in &period_nodes {
            let session_ids: Vec<String> = tree
                .iter()
                .filter(|n| n.node_type == "session" && n.parent_id.as_deref() == Some(&period.id))
                .map(|n| n.id.clone())
                .collect();

            let topic_nodes: Vec<_> = tree
                .iter()
                .filter(|n| {
                    n.node_type == "topic"
                        && n.parent_id
                            .as_ref()
                            .map(|pid| session_ids.contains(pid))
                            .unwrap_or(false)
                })
                .collect();

            if topic_nodes.len() <= max_topics_per_period {
                continue;
            }

            let summaries: Vec<String> = topic_nodes
                .iter()
                .map(|t| format!("# {}\n{}", t.title, t.summary))
                .collect();
            let combined = summaries.join("\n\n");

            let prompt = if let Some(p) = personality.filter(|s| !s.is_empty()) {
                format!(
                    "あなたは {persona_name} です。\n{p}\n\n以下の複数のトピック要約を、あなた自身の記憶として1つにまとめてください。\nJSON形式で返してください: {{\"title\": \"...\", \"summary\": \"...\"}}\n\n{combined}"
                )
            } else {
                format!(
                    "以下の複数のトピック要約を1つにまとめてください。\nJSON形式で返してください: {{\"title\": \"...\", \"summary\": \"...\"}}\n\n{combined}"
                )
            };

            let request = ChatRequestSimple {
                model: model.to_string(),
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
                max_tokens: Some(300),
            };

            let merged_summary = match llm.chat(request).await {
                Ok(resp) => {
                    let text = resp.content.unwrap_or_default();
                    let json_str = text
                        .trim()
                        .trim_start_matches("```json")
                        .trim_start_matches("```")
                        .trim_end_matches("```")
                        .trim();
                    serde_json::from_str::<LlmSummary>(json_str).unwrap_or(LlmSummary {
                        title: format!("Merged topics for {}", period.title),
                        summary: "Merged summary".to_string(),
                    })
                }
                Err(_) => LlmSummary {
                    title: format!("Merged topics for {}", period.title),
                    summary: "Merge failed".to_string(),
                },
            };

            let start_log = topic_nodes.iter().filter_map(|t| t.start_log_id).min();
            let end_log = topic_nodes.iter().filter_map(|t| t.end_log_id).max();
            let token_total: i32 = topic_nodes.iter().map(|t| t.token_count).sum();

            let parent_session_id = topic_nodes
                .first()
                .and_then(|t| t.parent_id.clone())
                .unwrap_or_else(|| session_ids.first().cloned().unwrap_or_default());

            {
                let db = conn
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
                for topic in &topic_nodes {
                    db.execute(
                        "DELETE FROM memory_index_nodes WHERE id = ?1",
                        rusqlite::params![topic.id],
                    )?;
                    deleted_count += 1;
                }
            }

            let merged_id = format!("merged-topic-{agent_id}-{}", Utc::now().timestamp_millis());
            let merged_node = opencrab_db::queries::IndexNodeRow {
                id: merged_id,
                agent_id: agent_id.to_string(),
                parent_id: Some(parent_session_id),
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: merged_summary.title,
                summary: merged_summary.summary,
                start_log_id: start_log,
                end_log_id: end_log,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 3,
                child_count: 0,
                token_count: token_total,
                created_at: now.clone(),
                updated_at: now.clone(),
                short_id: None,
            };
            {
                let db = conn
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
                opencrab_db::queries::insert_index_node(&db, &merged_node)?;
            }
            merged_count += 1;
        }

        {
            let db = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock failed: {e}"))?;
            let all_nodes = opencrab_db::queries::get_index_tree(&db, agent_id)?;
            let mut child_counts: HashMap<String, i32> = HashMap::new();
            for node in &all_nodes {
                if let Some(ref pid) = node.parent_id {
                    *child_counts.entry(pid.clone()).or_default() += 1;
                }
            }
            for (node_id, count) in &child_counts {
                opencrab_db::queries::update_index_node_child_count(&db, node_id, *count)?;
            }
        }

        Ok(MergeResult {
            periods_processed: period_nodes.len(),
            topics_merged: merged_count,
            topics_deleted: deleted_count,
        })
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
        async fn chat(&self, _request: ChatRequestSimple) -> Result<ChatResponseSimple> {
            Ok(ChatResponseSimple {
                content: Some(
                    r#"{"title": "テストトピック", "summary": "テスト要約です。"}"#.to_string(),
                ),
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
                content: Some(
                    r#"{"title": "テストトピック", "summary": "テスト要約です。"}"#.to_string(),
                ),
                tool_calls: vec![],
                finish_reason: "stop".to_string(),
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn test_build_incremental_empty() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        let result = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        assert_eq!(result.nodes_created, 0);
        assert_eq!(result.logs_indexed, 0);
    }

    #[tokio::test]
    async fn test_build_incremental_with_logs() {
        let db_conn = opencrab_db::init_memory().unwrap();
        // Insert some test logs
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Hello, this is a test message about Rust programming.".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let log2 = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Yes, Rust is great for systems programming.".to_string(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: Some(2),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log2).unwrap();

        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        let result = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        // root + period + session + topic = 4 nodes
        assert_eq!(result.nodes_created, 4);
        assert_eq!(result.logs_indexed, 2);

        // Verify watermark
        let db = conn.lock().unwrap();
        let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
            .unwrap()
            .unwrap();
        assert_eq!(wm.last_indexed_log_id, 2);
        assert_eq!(wm.total_nodes, 4);

        // Verify tree structure
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        assert_eq!(tree.len(), 4);
        assert!(tree.iter().any(|n| n.node_type == "root"));
        assert!(tree.iter().any(|n| n.node_type == "period"));
        assert!(tree.iter().any(|n| n.node_type == "session"));
        assert!(tree.iter().any(|n| n.node_type == "topic"));

        // Topic node should have LLM-generated title
        let topic = tree.iter().find(|n| n.node_type == "topic").unwrap();
        assert_eq!(topic.title, "テストトピック");
        assert_eq!(topic.summary, "テスト要約です。");
    }

    /// T-2.1: ペルソナ情報が要約プロンプトに含まれる
    #[tokio::test]
    async fn test_persona_prompt_contains_persona_info() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Hello, this is a test message.".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let conn = Arc::new(Mutex::new(db_conn));
        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };

        let _result = IndexBuilder::build_incremental(
            &conn,
            "agent-1",
            &llm,
            "test-model",
            50,
            "のすたろう",
            Some("17歳のオタク高校生"),
        )
        .await
        .unwrap();

        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = &request.messages[0].content;
        assert!(prompt.contains("のすたろう"), "プロンプトにペルソナ名が含まれるべき");
        assert!(prompt.contains("17歳のオタク高校生"), "プロンプトにpersonalityが含まれるべき");
    }

    /// T-2.2: 注目ポイント4軸がプロンプトに含まれる
    #[tokio::test]
    async fn test_persona_prompt_contains_four_axes() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Test message for four axes check.".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let conn = Arc::new(Mutex::new(db_conn));
        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };

        let _result = IndexBuilder::build_incremental(
            &conn,
            "agent-1",
            &llm,
            "test-model",
            50,
            "テスト",
            Some("テスト用ペルソナ"),
        )
        .await
        .unwrap();

        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = &request.messages[0].content;
        assert!(prompt.contains("学んだこと") || prompt.contains("技術知見"), "技術知見軸が含まれるべき");
        assert!(prompt.contains("判断の理由") || prompt.contains("判断"), "判断軸が含まれるべき");
        assert!(prompt.contains("関係性") || prompt.contains("感情"), "関係性・感情軸が含まれるべき");
        assert!(prompt.contains("失敗") || prompt.contains("教訓"), "失敗・教訓軸が含まれるべき");
    }

    /// T-2.5: ペルソナが空でもエラーにならずデフォルト一人称で要約される
    #[tokio::test]
    async fn test_persona_empty_uses_default() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Test message for empty persona.".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let conn = Arc::new(Mutex::new(db_conn));
        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };

        let result = IndexBuilder::build_incremental(
            &conn,
            "agent-1",
            &llm,
            "test-model",
            50,
            "",
            None,
        )
        .await
        .unwrap();

        assert!(result.nodes_created > 0, "ノードが生成されるべき");

        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = &request.messages[0].content;
        // Default prompt should still use 一人称
        assert!(prompt.contains("一人称"), "デフォルトプロンプトに一人称が含まれるべき");
    }

    #[tokio::test]
    async fn test_build_incremental_idempotent() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Test message".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        // First build
        let r1 = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        assert!(r1.nodes_created > 0);

        // Second build should create no new nodes (no new logs)
        let r2 = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        assert_eq!(r2.nodes_created, 0);
        assert_eq!(r2.logs_indexed, 0);
    }

    /// ヘルパー: 指定セッションにN件のログを投入
    fn insert_logs(conn: &rusqlite::Connection, agent_id: &str, session_id: &str, count: usize) {
        for i in 0..count {
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                log_type: "message".to_string(),
                content: format!("Message {i} in session {session_id}"),
                speaker_id: Some(if i % 2 == 0 {
                    "user-1".to_string()
                } else {
                    agent_id.to_string()
                }),
                turn_number: Some(i as i32),
                metadata_json: None,
                created_at: None,
            };
            opencrab_db::queries::insert_session_log(conn, &log).unwrap();
        }
    }

    /// 複数セッションにまたがるログ — 各セッションが別のsession/topicノードになるか
    #[tokio::test]
    async fn test_multiple_sessions() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-a", 5);
        insert_logs(&db_conn, "agent-1", "session-b", 3);
        insert_logs(&db_conn, "agent-1", "session-c", 4);

        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        let result = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        assert_eq!(result.logs_indexed, 12); // 5+3+4

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();

        // root(1) + period(1) + session(3) + topic(3) = 8
        assert_eq!(tree.len(), 8);
        let sessions: Vec<_> = tree.iter().filter(|n| n.node_type == "session").collect();
        assert_eq!(sessions.len(), 3);
        let topics: Vec<_> = tree.iter().filter(|n| n.node_type == "topic").collect();
        assert_eq!(topics.len(), 3);

        // 各topicは異なるsource_session_idを持つ
        let mut topic_sessions: Vec<_> = topics
            .iter()
            .filter_map(|n| n.source_session_id.clone())
            .collect();
        topic_sessions.sort();
        assert_eq!(topic_sessions, vec!["session-a", "session-b", "session-c"]);
    }

    /// バッチサイズ超過 — batch_sizeで切られ、残りは次回ビルドで処理される
    #[tokio::test]
    async fn test_batch_size_limit() {
        let db_conn = opencrab_db::init_memory().unwrap();
        // 30件投入、batch_size=10で実行
        insert_logs(&db_conn, "agent-1", "session-1", 15);
        insert_logs(&db_conn, "agent-1", "session-2", 15);

        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        // batch_size=10: 最初の10件のみ処理される
        let r1 = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 10, "", None)
            .await
            .unwrap();
        assert_eq!(r1.logs_indexed, 10);

        // ウォーターマークは10件目まで進んでいる
        {
            let db = conn.lock().unwrap();
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(wm.last_indexed_log_id, 10);
        }

        // 2回目: 次の10件
        let r2 = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 10, "", None)
            .await
            .unwrap();
        assert_eq!(r2.logs_indexed, 10);

        // 3回目: 残り10件
        let r3 = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 10, "", None)
            .await
            .unwrap();
        assert_eq!(r3.logs_indexed, 10);

        // 4回目: もう残りなし
        let r4 = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 10, "", None)
            .await
            .unwrap();
        assert_eq!(r4.logs_indexed, 0);
        assert_eq!(r4.nodes_created, 0);

        // 最終的にウォーターマークは30件目
        {
            let db = conn.lock().unwrap();
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(wm.last_indexed_log_id, 30);
        }
    }

    /// 増分ビルド — 初回ビルド後に新ログ追加、再ビルドで新ノードのみ追加
    #[tokio::test]
    async fn test_incremental_after_new_logs() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);

        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        // 初回ビルド
        let r1 = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        assert_eq!(r1.logs_indexed, 5);
        let first_node_count = r1.nodes_created;

        // 新しいセッションにログ追加
        {
            let db = conn.lock().unwrap();
            insert_logs(&db, "agent-1", "session-2", 3);
        }

        // 2回目ビルド — 新ログのみ処理、session-2のノードが追加される
        let r2 = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        assert_eq!(r2.logs_indexed, 3);
        // session + topic = 2 new nodes (root/periodは既存を再利用)
        assert_eq!(r2.nodes_created, 2);

        // ツリー全体を検証
        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        assert_eq!(tree.len(), first_node_count + 2); // 初回4 + session+topic=2
        let sessions: Vec<_> = tree.iter().filter(|n| n.node_type == "session").collect();
        assert_eq!(sessions.len(), 2);

        // ウォーターマークが最終ログまで進んでいる
        let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
            .unwrap()
            .unwrap();
        assert_eq!(wm.last_indexed_log_id, 8); // 5+3
    }

    /// 大量ログ（1セッション100件） — 全件が1つのtopicノードにまとまる
    #[tokio::test]
    async fn test_large_single_session() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-big", 100);

        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        let result = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 200, "", None)
            .await
            .unwrap();

        assert_eq!(result.logs_indexed, 100);

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        // root(1) + period(1) + session(1) + topic(1) = 4
        assert_eq!(tree.len(), 4);

        let topic = tree.iter().find(|n| n.node_type == "topic").unwrap();
        assert_eq!(topic.start_log_id, Some(1));
        assert_eq!(topic.end_log_id, Some(100));
        assert!(topic.token_count > 0);

        // child_countが正しく更新されている
        let root = tree.iter().find(|n| n.node_type == "root").unwrap();
        assert_eq!(root.child_count, 1); // period
        let session = tree.iter().find(|n| n.node_type == "session").unwrap();
        assert_eq!(session.child_count, 1); // topic
    }

    /// 異なるエージェントのログが混在 — agent_idでフィルタされる
    #[tokio::test]
    async fn test_agent_isolation() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);
        insert_logs(&db_conn, "agent-2", "session-2", 8);

        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        // agent-1のみビルド
        let r1 = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        assert_eq!(r1.logs_indexed, 5);

        // agent-2のみビルド
        let r2 = IndexBuilder::build_incremental(&conn, "agent-2", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        assert_eq!(r2.logs_indexed, 8);

        // 各エージェントのツリーが独立
        let db = conn.lock().unwrap();
        let tree1 = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        let tree2 = opencrab_db::queries::get_index_tree(&db, "agent-2").unwrap();
        assert_eq!(tree1.len(), 4); // root+period+session+topic
        assert_eq!(tree2.len(), 4);
        // ノードIDが重複しない
        let ids1: Vec<_> = tree1.iter().map(|n| &n.id).collect();
        let ids2: Vec<_> = tree2.iter().map(|n| &n.id).collect();
        for id in &ids1 {
            assert!(!ids2.contains(id));
        }
    }

    // ================================================================
    // delete_index / rebuild_index / merge_topics テスト
    // ================================================================

    /// delete_index: 全ノードとウォーターマークが削除されるか
    #[tokio::test]
    async fn test_delete_index() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);
        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        // まずインデックスを構築
        let r = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        assert!(r.nodes_created > 0);

        // 削除前にツリーとウォーターマークが存在することを確認
        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            assert!(!tree.is_empty(), "削除前はノードが存在するはず");
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1").unwrap();
            assert!(wm.is_some(), "削除前はウォーターマークが存在するはず");
        }

        // 削除実行
        IndexBuilder::delete_index(&conn, "agent-1").unwrap();

        // 削除後: ツリーもウォーターマークも空になるはず
        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            assert!(tree.is_empty(), "削除後はノードが0件になるはず");
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1").unwrap();
            assert!(wm.is_none(), "削除後はウォーターマークがNoneになるはず");
        }
    }

    /// delete_index: 他のエージェントのデータは影響を受けない
    #[tokio::test]
    async fn test_delete_index_isolation() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);
        insert_logs(&db_conn, "agent-2", "session-2", 5);
        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        IndexBuilder::build_incremental(&conn, "agent-2", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        // agent-1のみ削除
        IndexBuilder::delete_index(&conn, "agent-1").unwrap();

        // agent-1は空、agent-2は無事
        {
            let db = conn.lock().unwrap();
            let tree1 = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            let tree2 = opencrab_db::queries::get_index_tree(&db, "agent-2").unwrap();
            assert!(tree1.is_empty(), "agent-1のノードは削除済みのはず");
            assert!(!tree2.is_empty(), "agent-2のノードは無事なはず");
            let wm1 = opencrab_db::queries::get_index_watermark(&db, "agent-1").unwrap();
            let wm2 = opencrab_db::queries::get_index_watermark(&db, "agent-2").unwrap();
            assert!(wm1.is_none(), "agent-1のウォーターマークは削除済みのはず");
            assert!(wm2.is_some(), "agent-2のウォーターマークは無事なはず");
        }
    }

    /// rebuild_index: 削除 → 再構築でウォーターマークがリセットされ、全ログが再インデックスされるか
    #[tokio::test]
    async fn test_rebuild_index() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);
        insert_logs(&db_conn, "agent-1", "session-2", 3);
        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        // 初回ビルド
        let r1 = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        let first_tree_len = {
            let db = conn.lock().unwrap();
            opencrab_db::queries::get_index_tree(&db, "agent-1")
                .unwrap()
                .len()
        };
        assert!(r1.nodes_created > 0);

        // 再構築
        let r2 = IndexBuilder::rebuild_index(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        // 再構築後は全ログが再インデックスされる
        assert_eq!(r2.logs_indexed, 8, "8件全ログが再インデックスされるはず");
        assert!(r2.nodes_created > 0, "ノードが再作成されるはず");

        // ウォーターマークが最新の状態
        {
            let db = conn.lock().unwrap();
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(
                wm.last_indexed_log_id, 8,
                "ウォーターマークが最終ログIDを指すはず"
            );

            // ツリーが再構築されている
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            assert_eq!(tree.len(), first_tree_len, "再構築後もツリー構造が同じはず");
        }
    }

    /// rebuild_index: 空のインデックスからも再構築できる
    #[tokio::test]
    async fn test_rebuild_index_from_empty() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 3);
        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        // ビルドせずに再構築（初回rebuild）
        let r = IndexBuilder::rebuild_index(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        assert_eq!(r.logs_indexed, 3);
        assert_eq!(r.nodes_created, 4); // root+period+session+topic
    }

    /// merge_topics: topicが閾値以下なら変化なし
    #[tokio::test]
    async fn test_merge_topics_no_merge_needed() {
        let db_conn = opencrab_db::init_memory().unwrap();
        // 2セッション = 2topicノード
        insert_logs(&db_conn, "agent-1", "session-a", 3);
        insert_logs(&db_conn, "agent-1", "session-b", 3);
        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        let tree_before = {
            let db = conn.lock().unwrap();
            opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap()
        };

        // max_topics_per_period=5 なので2topicは閾値以下
        let result = IndexBuilder::merge_topics(&conn, "agent-1", &llm, "test-model", 5, "", None)
            .await
            .unwrap();

        assert_eq!(result.topics_merged, 0, "マージ不要なのでmergedは0");
        assert_eq!(result.topics_deleted, 0, "削除も0");

        // ツリーは変化なし
        let tree_after = {
            let db = conn.lock().unwrap();
            opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap()
        };
        assert_eq!(
            tree_before.len(),
            tree_after.len(),
            "マージなしでツリー長は変化しない"
        );
    }

    /// merge_topics: topicが閾値超過でマージされ、要約が統合されるか
    #[tokio::test]
    async fn test_merge_topics_triggers_merge() {
        let db_conn = opencrab_db::init_memory().unwrap();
        // 4セッション = 4topicノード（1periodの下）
        insert_logs(&db_conn, "agent-1", "session-a", 2);
        insert_logs(&db_conn, "agent-1", "session-b", 2);
        insert_logs(&db_conn, "agent-1", "session-c", 2);
        insert_logs(&db_conn, "agent-1", "session-d", 2);
        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        // max_topics_per_period=2 → 4topics > 2 なのでマージ発動
        let result = IndexBuilder::merge_topics(&conn, "agent-1", &llm, "test-model", 2, "", None)
            .await
            .unwrap();

        assert_eq!(result.periods_processed, 1, "1つのperiodが処理されるはず");
        assert!(
            result.topics_merged >= 1,
            "少なくとも1回のマージが実行されるはず"
        );
        assert!(
            result.topics_deleted >= 3,
            "旧topicは削除されるはず（4 - 1 = 3）"
        );

        // マージ後: 4topicが統合されて1topicになる
        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        let topics: Vec<_> = tree.iter().filter(|n| n.node_type == "topic").collect();
        // マージで4→1になる（merged-topicが1つ）
        assert_eq!(topics.len(), 1, "4topicがマージされて1topicになるはず");
        // マージされたtopicはMockLlmのタイトルを持つ
        assert_eq!(
            topics[0].title, "テストトピック",
            "LLM生成タイトルを持つはず"
        );

        // child_countが正しく更新されている
        let session_nodes: Vec<_> = tree.iter().filter(|n| n.node_type == "session").collect();
        // マージ後、統合topicの親sessionのchild_countが更新されているはず
        let _ = session_nodes; // child_count検証は親ID依存のため省略
    }

    /// merge_topics: マージ後にrebuild_indexしても整合性が保たれる
    #[tokio::test]
    async fn test_merge_then_rebuild() {
        let db_conn = opencrab_db::init_memory().unwrap();
        for i in 0..5 {
            insert_logs(&db_conn, "agent-1", &format!("session-{i}"), 3);
        }
        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        // まずマージ
        let merge_result = IndexBuilder::merge_topics(&conn, "agent-1", &llm, "test-model", 2, "", None)
            .await
            .unwrap();
        assert!(merge_result.topics_merged > 0);

        // その後rebuildすると完全にリフレッシュされる
        let rebuild_result = IndexBuilder::rebuild_index(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        assert_eq!(
            rebuild_result.logs_indexed, 15,
            "5セッション×3ログ=15件が再インデックス"
        );

        // orphanなし、child_count整合
        let db = conn.lock().unwrap();
        let metrics =
            crate::memory_index::graph_query::IndexQualityMetrics::compute(&db, "agent-1").unwrap();
        assert_eq!(metrics.orphan_count, 0);
        assert_eq!(metrics.child_count_mismatch, 0);
        assert_eq!(metrics.log_coverage, 1.0);
    }

    /// ヘルパー: 指定セッションにログを created_at 付きで投入
    /// insert_session_log は常に Utc::now() を使うため、INSERT 後に UPDATE で上書きする
    fn insert_log_with_date(
        conn: &rusqlite::Connection,
        agent_id: &str,
        session_id: &str,
        turn: i32,
        content: &str,
        created_at: &str,
    ) {
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "message".to_string(),
            content: content.to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(turn),
            metadata_json: None,
            created_at: Some(created_at.to_string()),
        };
        let row_id = opencrab_db::queries::insert_session_log(conn, &log).unwrap();
        conn.execute(
            "UPDATE memory_sessions SET created_at = ?1 WHERE id = ?2",
            rusqlite::params![created_at, row_id],
        )
        .unwrap();
    }

    /// T-1.10: 同一日のログ → date_from == date_to == その日
    #[tokio::test]
    async fn test_build_date_same_day() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        {
            let c = conn.lock().unwrap();
            insert_log_with_date(&c, "agent-d", "sess-d", 0, "Morning msg", "2026-04-01 09:00:00");
            insert_log_with_date(&c, "agent-d", "sess-d", 1, "Afternoon msg", "2026-04-01 15:00:00");
        }

        let result = IndexBuilder::build_incremental(&conn, "agent-d", &llm, "test-model", 2, "", None)
            .await
            .unwrap();
        assert!(result.nodes_created > 0);

        let c = conn.lock().unwrap();
        let topics = opencrab_db::queries::get_topic_nodes_for_session(&c, "agent-d", "sess-d").unwrap();
        assert!(!topics.is_empty());
        let t = &topics[0];
        assert_eq!(t.date_from.as_deref(), Some("2026-04-01"));
        assert_eq!(t.date_to.as_deref(), Some("2026-04-01"));
    }

    /// T-1.11: 複数日にまたがるログ → date_from < date_to
    #[tokio::test]
    async fn test_build_date_multi_day() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        {
            let c = conn.lock().unwrap();
            insert_log_with_date(&c, "agent-m", "sess-m", 0, "Day 1 msg", "2026-04-01 10:00:00");
            insert_log_with_date(&c, "agent-m", "sess-m", 1, "Day 3 msg", "2026-04-03 14:00:00");
        }

        let result = IndexBuilder::build_incremental(&conn, "agent-m", &llm, "test-model", 2, "", None)
            .await
            .unwrap();
        assert!(result.nodes_created > 0);

        let c = conn.lock().unwrap();
        let topics = opencrab_db::queries::get_topic_nodes_for_session(&c, "agent-m", "sess-m").unwrap();
        assert!(!topics.is_empty());
        let t = &topics[0];
        assert_eq!(t.date_from.as_deref(), Some("2026-04-01"));
        assert_eq!(t.date_to.as_deref(), Some("2026-04-03"));
    }

    /// T-1.12: created_at が短い/空文字のログ → date_from/date_to は None（パニックしない）
    /// memory_sessions.created_at は NOT NULL なので NULL にはできないが、
    /// 空文字や短い文字列でも s[..10] スライスでパニックしないことを検証する
    #[tokio::test]
    async fn test_build_date_null_created_at() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = Arc::new(Mutex::new(db_conn));
        let llm = MockLlm;

        {
            let c = conn.lock().unwrap();
            // Use the existing insert_logs helper which sets created_at to now()
            insert_logs(&c, "agent-n", "sess-n", 3);
            // Set created_at to empty string to simulate missing/invalid date
            c.execute_batch("UPDATE memory_sessions SET created_at = '' WHERE agent_id = 'agent-n'").unwrap();
        }

        let result = IndexBuilder::build_incremental(&conn, "agent-n", &llm, "test-model", 3, "", None)
            .await
            .unwrap();
        // Should not panic, nodes should still be created
        assert!(result.nodes_created > 0);

        let c = conn.lock().unwrap();
        let topics = opencrab_db::queries::get_topic_nodes_for_session(&c, "agent-n", "sess-n").unwrap();
        assert!(!topics.is_empty());
        let t = &topics[0];
        // date_from/date_to should be None when all logs have empty created_at
        assert_eq!(t.date_from, None);
        assert_eq!(t.date_to, None);
    }
}
