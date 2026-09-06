//! memory_index テスト群で共有する topic ノード投入・件数ヘルパ。
//! 2 つ以上の子モジュールから使われるものだけを置く（1 モジュール専用は各自が持つ）。

use super::*;

pub(super) fn mk_topic_node(
    id: &str,
    agent_id: &str,
    title: &str,
    summary: &str,
    keywords: &[&str],
) -> IndexNodeRow {
    IndexNodeRow {
        id: id.to_string(),
        agent_id: agent_id.to_string(),
        parent_id: None,
        node_type: "topic".to_string(),
        source_type: "session_log".to_string(),
        title: title.to_string(),
        summary: summary.to_string(),
        start_log_id: None,
        end_log_id: None,
        source_session_id: None,
        date_from: Some("2026-06-01".to_string()),
        date_to: Some("2026-06-02".to_string()),
        depth: 3,
        child_count: 0,
        token_count: 0,
        created_at: "2026-06-01T00:00:00Z".to_string(),
        updated_at: "2026-06-01T00:00:00Z".to_string(),
        short_id: Some(id.to_string()),
        keywords_json: serde_json::to_string(keywords).unwrap(),
        summary_refreshed_at: None,
    }
}

/// テスト用に topic ノードを 1 件積む（`node_type='topic'`, `source_type='session_log'`）。
pub(super) fn insert_test_topic(conn: &Connection, agent_id: &str, id: &str, short_id: &str) {
    insert_index_node(
        conn,
        &IndexNodeRow {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
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
            created_at: "2026-06-01T00:00:00Z".to_string(),
            updated_at: "2026-06-01T00:00:00Z".to_string(),
            short_id: Some(short_id.to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
}

/// テスト用に topic ノードを 1 件入れる（created_at / end_log_id / source_type を指定）。
pub(super) fn seed_topic(
    conn: &Connection,
    agent_id: &str,
    id: &str,
    short: &str,
    created_at: &str,
    end_log_id: Option<i64>,
    source_type: &str,
) {
    insert_index_node(
        conn,
        &IndexNodeRow {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: source_type.to_string(),
            title: format!("題 {short}"),
            summary: "s".to_string(),
            start_log_id: None,
            end_log_id,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 3,
            child_count: 0,
            token_count: 0,
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
            short_id: Some(short.to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
}

pub(super) fn members_for(conn: &Connection, agent: &str, topic_id: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM memory_category_members WHERE agent_id = ?1 AND topic_id = ?2",
        params![agent, topic_id],
        |r| r.get(0),
    )
    .unwrap()
}
