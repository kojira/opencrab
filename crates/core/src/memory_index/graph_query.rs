//! インデックス品質メトリクスの計算。

use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// インデックスの品質メトリクスを計算する。テスト・運用時の精度評価に使用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexQualityMetrics {
    /// ツリー内の全ノード数
    pub total_nodes: usize,
    /// ノードタイプ別カウント
    pub nodes_by_type: HashMap<String, usize>,
    /// orphan（parent_idが存在しないノード、rootを除く）の数
    pub orphan_count: usize,
    /// child_countと実際の子ノード数が一致しないノード数
    pub child_count_mismatch: usize,
    /// インデックスでカバーされるログID数
    pub covered_log_ids: usize,
    /// 全ログ数
    pub total_logs: usize,
    /// カバレッジ率 (0.0 - 1.0)
    pub log_coverage: f64,
    /// depth別ノード数
    pub nodes_by_depth: HashMap<i32, usize>,
    /// summaryが空のノード数
    pub empty_summary_count: usize,
    /// titleが空のノード数
    pub empty_title_count: usize,
    /// 平均summary長（文字数）
    pub avg_summary_length: f64,
    /// 最大ツリー深さ
    pub max_depth: i32,
}

impl IndexQualityMetrics {
    /// DBからインデックスの品質メトリクスを計算する。
    pub fn compute(conn: &rusqlite::Connection, agent_id: &str) -> Result<Self> {
        let tree = opencrab_db::queries::get_index_tree(conn, agent_id)?;
        let node_ids: std::collections::HashSet<String> =
            tree.iter().map(|n| n.id.clone()).collect();

        // ノードタイプ別カウント
        let mut nodes_by_type: HashMap<String, usize> = HashMap::new();
        let mut nodes_by_depth: HashMap<i32, usize> = HashMap::new();
        let mut actual_children: HashMap<String, usize> = HashMap::new();
        let mut orphan_count = 0;
        let mut empty_summary_count = 0;
        let mut empty_title_count = 0;
        let mut summary_total_len: usize = 0;
        let mut max_depth = 0;

        for node in &tree {
            *nodes_by_type.entry(node.node_type.clone()).or_default() += 1;
            *nodes_by_depth.entry(node.depth).or_default() += 1;
            if node.depth > max_depth {
                max_depth = node.depth;
            }
            if node.summary.is_empty() {
                empty_summary_count += 1;
            }
            if node.title.is_empty() {
                empty_title_count += 1;
            }
            summary_total_len += node.summary.len();

            if let Some(ref pid) = node.parent_id {
                *actual_children.entry(pid.clone()).or_default() += 1;
                if !node_ids.contains(pid) {
                    orphan_count += 1;
                }
            }
        }

        // child_countの整合性チェック
        let mut child_count_mismatch = 0;
        for node in &tree {
            let actual = actual_children.get(&node.id).copied().unwrap_or(0);
            if actual != node.child_count as usize {
                child_count_mismatch += 1;
            }
        }

        // ログカバレッジ計算
        let total_logs = {
            let _wm = opencrab_db::queries::get_index_watermark(conn, agent_id)?;
            // watermarkの最終IDではなく実際のログ数をカウント
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM memory_sessions WHERE agent_id = ?1",
                rusqlite::params![agent_id],
                |row| row.get(0),
            )?;
            count as usize
        };

        // topicノードのlog_idレンジからカバーされるログ数を推定
        let mut covered_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for node in &tree {
            if let (Some(start), Some(end)) = (node.start_log_id, node.end_log_id) {
                for id in start..=end {
                    covered_ids.insert(id);
                }
            }
        }
        // 実際にそのagentのログIDと交差する数
        let covered_log_ids = if total_logs > 0 {
            let mut stmt = conn.prepare("SELECT id FROM memory_sessions WHERE agent_id = ?1")?;
            let real_ids: Vec<i64> = stmt
                .query_map(rusqlite::params![agent_id], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();
            real_ids
                .iter()
                .filter(|id| covered_ids.contains(id))
                .count()
        } else {
            0
        };

        let log_coverage = if total_logs > 0 {
            covered_log_ids as f64 / total_logs as f64
        } else {
            1.0
        };

        let avg_summary_length = if tree.is_empty() {
            0.0
        } else {
            summary_total_len as f64 / tree.len() as f64
        };

        Ok(Self {
            total_nodes: tree.len(),
            nodes_by_type,
            orphan_count,
            child_count_mismatch,
            covered_log_ids,
            total_logs,
            log_coverage,
            nodes_by_depth,
            empty_summary_count,
            empty_title_count,
            avg_summary_length,
            max_depth,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::LlmClient;
    use crate::engine::{ChatRequest, ChatResponse};
    use crate::memory_index::index_builder::IndexBuilder;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

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

    struct MockLlm;

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse::text(
                r#"{"title": "テストトピック", "summary": "テスト要約です。"}"#.to_string(),
            ))
        }
    }

    /// 基本的な品質メトリクスの検証
    #[tokio::test]
    async fn test_quality_metrics_basic() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 10);
        insert_logs(&db_conn, "agent-1", "session-2", 8);

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let metrics = IndexQualityMetrics::compute(&db, "agent-1").unwrap();

        // 構造の正しさ
        assert_eq!(metrics.orphan_count, 0, "orphanノードがあってはならない");
        assert_eq!(metrics.child_count_mismatch, 0, "child_countが不整合");
        assert_eq!(metrics.empty_title_count, 0, "空タイトルがあってはならない");
        assert_eq!(
            metrics.empty_summary_count, 0,
            "空サマリーがあってはならない"
        );

        // カバレッジ
        assert_eq!(metrics.total_logs, 18);
        assert_eq!(metrics.covered_log_ids, 18, "全ログがカバーされるべき");
        assert!(
            (metrics.log_coverage - 1.0).abs() < f64::EPSILON,
            "カバレッジ100%"
        );

        // ノードタイプ分布
        assert_eq!(metrics.nodes_by_type.get("root"), Some(&1));
        assert_eq!(metrics.nodes_by_type.get("period"), Some(&1));
        assert_eq!(metrics.nodes_by_type.get("session"), Some(&2));
        assert_eq!(metrics.nodes_by_type.get("topic"), Some(&2));

        // 深さ分布: root(0), period(1), session(2), topic(3)
        assert_eq!(metrics.max_depth, 3);
        assert_eq!(metrics.nodes_by_depth.get(&0), Some(&1));
        assert_eq!(metrics.nodes_by_depth.get(&3), Some(&2));
    }

    /// バッチ分割後の品質 — 複数回ビルドしてもカバレッジ100%になるか
    #[tokio::test]
    async fn test_quality_after_batched_builds() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 20);
        insert_logs(&db_conn, "agent-1", "session-2", 15);
        insert_logs(&db_conn, "agent-1", "session-3", 10);

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // batch_size=15 で3回に分けてビルド
        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 15, "", None)
            .await
            .unwrap();
        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 15, "", None)
            .await
            .unwrap();
        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 15, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let metrics = IndexQualityMetrics::compute(&db, "agent-1").unwrap();

        assert_eq!(metrics.total_logs, 45);
        assert_eq!(
            metrics.log_coverage, 1.0,
            "バッチ分割後も全ログがカバーされるべき (coverage: {})",
            metrics.log_coverage
        );
        assert_eq!(metrics.orphan_count, 0);
        assert_eq!(metrics.child_count_mismatch, 0);
        assert_eq!(*metrics.nodes_by_type.get("session").unwrap_or(&0), 3);
    }

    /// 増分ビルド後の品質 — 時間経過で新ログが増えてもカバレッジが維持される
    #[tokio::test]
    async fn test_quality_incremental_coverage() {
        let db_conn = opencrab_db::init_memory().unwrap();

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // フェーズ1: 初回ログ + ビルド
        {
            let db = conn.lock().unwrap();
            insert_logs(&db, "agent-1", "session-1", 10);
        }
        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        {
            let db = conn.lock().unwrap();
            let m = IndexQualityMetrics::compute(&db, "agent-1").unwrap();
            assert_eq!(m.log_coverage, 1.0);
            assert_eq!(m.total_logs, 10);
        }

        // フェーズ2: 新ログ追加 + ビルド
        {
            let db = conn.lock().unwrap();
            insert_logs(&db, "agent-1", "session-2", 15);
        }
        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        {
            let db = conn.lock().unwrap();
            let m = IndexQualityMetrics::compute(&db, "agent-1").unwrap();
            assert_eq!(m.log_coverage, 1.0);
            assert_eq!(m.total_logs, 25);
        }

        // フェーズ3: さらに追加
        {
            let db = conn.lock().unwrap();
            insert_logs(&db, "agent-1", "session-3", 20);
            insert_logs(&db, "agent-1", "session-4", 5);
        }
        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let metrics = IndexQualityMetrics::compute(&db, "agent-1").unwrap();
        assert_eq!(metrics.total_logs, 50);
        assert_eq!(metrics.log_coverage, 1.0, "3フェーズ後もカバレッジ100%");
        assert_eq!(metrics.orphan_count, 0);
        assert_eq!(metrics.child_count_mismatch, 0);
        assert_eq!(*metrics.nodes_by_type.get("session").unwrap_or(&0), 4);
        assert_eq!(*metrics.nodes_by_type.get("topic").unwrap_or(&0), 4);
        assert!(metrics.avg_summary_length > 0.0, "平均サマリー長が正");
    }

    /// 大規模テスト — 多数のセッション・大量ログでの品質
    #[tokio::test]
    async fn test_quality_large_scale() {
        let db_conn = opencrab_db::init_memory().unwrap();

        // 20セッション × 各25件 = 500件のログ
        for i in 0..20 {
            insert_logs(&db_conn, "agent-1", &format!("session-{i:03}"), 25);
        }

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // batch_size=100 で5回ビルド
        for _ in 0..5 {
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 100, "", None)
                .await
                .unwrap();
        }

        let db = conn.lock().unwrap();
        let metrics = IndexQualityMetrics::compute(&db, "agent-1").unwrap();

        assert_eq!(metrics.total_logs, 500);
        assert_eq!(metrics.log_coverage, 1.0, "大規模でもカバレッジ100%");
        assert_eq!(metrics.orphan_count, 0, "大規模でもorphanなし");
        assert_eq!(metrics.child_count_mismatch, 0, "大規模でもchild_count整合");
        assert_eq!(*metrics.nodes_by_type.get("root").unwrap_or(&0), 1);
        assert_eq!(*metrics.nodes_by_type.get("session").unwrap_or(&0), 20);
        assert_eq!(*metrics.nodes_by_type.get("topic").unwrap_or(&0), 20);
        assert_eq!(metrics.empty_title_count, 0);
        assert_eq!(metrics.empty_summary_count, 0);

        // ツリー構造: root(1) + period(1) + session(20) + topic(20) = 42
        assert_eq!(metrics.total_nodes, 42);
    }

    /// エージェント間の隔離が品質メトリクスでも保たれるか
    #[tokio::test]
    async fn test_quality_agent_isolation() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "s1", 10);
        insert_logs(&db_conn, "agent-2", "s2", 20);

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        IndexBuilder::build_incremental(&conn, "agent-2", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let m1 = IndexQualityMetrics::compute(&db, "agent-1").unwrap();
        let m2 = IndexQualityMetrics::compute(&db, "agent-2").unwrap();

        // 各エージェントが自分のログだけカバー
        assert_eq!(m1.total_logs, 10);
        assert_eq!(m1.covered_log_ids, 10);
        assert_eq!(m1.log_coverage, 1.0);
        assert_eq!(m2.total_logs, 20);
        assert_eq!(m2.covered_log_ids, 20);
        assert_eq!(m2.log_coverage, 1.0);

        // ノード数が独立
        assert_eq!(m1.total_nodes, 4); // root+period+session+topic
        assert_eq!(m2.total_nodes, 4);
    }
}
