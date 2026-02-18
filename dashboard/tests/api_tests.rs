//! ダッシュボードAPIテスト
//!
//! サーバー関数が使うDB操作とDTO変換をテストする。
//! Dioxusランタイムなしで動作するテスト。

use opencrab_db;
use rusqlite::Connection;

// ============================================
// DTOシリアライズ/デシリアライズ
// ============================================

#[test]
fn test_agent_summary_serde() {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
    struct AgentSummary {
        id: String,
        name: String,
        persona_name: String,
        role: String,
        image_url: Option<String>,
        status: String,
        skill_count: i32,
        session_count: i32,
    }

    let agent = AgentSummary {
        id: "agent-1".to_string(),
        name: "Kai".to_string(),
        persona_name: "Pragmatic Engineer".to_string(),
        role: "discussant".to_string(),
        image_url: None,
        status: "idle".to_string(),
        skill_count: 3,
        session_count: 1,
    };

    let json = serde_json::to_string(&agent).unwrap();
    let deserialized: AgentSummary = serde_json::from_str(&json).unwrap();
    assert_eq!(agent, deserialized);
}

#[test]
fn test_personality_dto_serde() {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
    struct PersonalityDto {
        openness: f32,
        conscientiousness: f32,
        extraversion: f32,
        agreeableness: f32,
        neuroticism: f32,
    }

    let personality = PersonalityDto {
        openness: 0.8,
        conscientiousness: 0.6,
        extraversion: 0.4,
        agreeableness: 0.7,
        neuroticism: 0.2,
    };

    let json = serde_json::to_string(&personality).unwrap();
    let deserialized: PersonalityDto = serde_json::from_str(&json).unwrap();
    assert_eq!(personality, deserialized);
}

#[test]
fn test_session_dto_serde() {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
    struct SessionDto {
        id: String,
        mode: String,
        theme: String,
        phase: String,
        turn_number: i32,
        status: String,
        participant_count: usize,
    }

    let session = SessionDto {
        id: "session-1".to_string(),
        mode: "discussion".to_string(),
        theme: "AI Ethics".to_string(),
        phase: "main".to_string(),
        turn_number: 5,
        status: "active".to_string(),
        participant_count: 3,
    };

    let json = serde_json::to_string(&session).unwrap();
    let deserialized: SessionDto = serde_json::from_str(&json).unwrap();
    assert_eq!(session, deserialized);
}

#[test]
fn test_llm_metrics_summary_dto_serde() {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
    struct LlmMetricsSummaryDto {
        count: i64,
        total_tokens: i64,
        total_cost: f64,
        avg_latency: f64,
        avg_quality: f64,
    }

    let summary = LlmMetricsSummaryDto {
        count: 100,
        total_tokens: 50000,
        total_cost: 1.25,
        avg_latency: 350.5,
        avg_quality: 0.85,
    };

    let json = serde_json::to_string(&summary).unwrap();
    let deserialized: LlmMetricsSummaryDto = serde_json::from_str(&json).unwrap();
    assert_eq!(summary, deserialized);
}

#[test]
fn test_workspace_entry_dto_serde() {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
    struct WorkspaceEntryDto {
        name: String,
        is_dir: bool,
        size: u64,
    }

    let entry = WorkspaceEntryDto {
        name: "notes.txt".to_string(),
        is_dir: false,
        size: 1024,
    };

    let json = serde_json::to_string(&entry).unwrap();
    let deserialized: WorkspaceEntryDto = serde_json::from_str(&json).unwrap();
    assert_eq!(entry, deserialized);

    let dir = WorkspaceEntryDto {
        name: "subdir".to_string(),
        is_dir: true,
        size: 0,
    };
    let json = serde_json::to_string(&dir).unwrap();
    let deserialized: WorkspaceEntryDto = serde_json::from_str(&json).unwrap();
    assert_eq!(dir, deserialized);
}

// ============================================
// ヘルパー関数テスト
// ============================================

fn format_number(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}

#[test]
fn test_format_number_small() {
    assert_eq!(format_number(0), "0");
    assert_eq!(format_number(42), "42");
    assert_eq!(format_number(999), "999");
}

#[test]
fn test_format_number_thousands() {
    assert_eq!(format_number(1_000), "1.0K");
    assert_eq!(format_number(1_500), "1.5K");
    assert_eq!(format_number(999_999), "1000.0K");
}

#[test]
fn test_format_number_millions() {
    assert_eq!(format_number(1_000_000), "1.0M");
    assert_eq!(format_number(2_500_000), "2.5M");
    assert_eq!(format_number(10_000_000), "10.0M");
}

// ============================================
// DB操作テスト（サーバー関数のロジック相当）
// ============================================

fn setup_test_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    opencrab_db::schema::initialize(&conn).unwrap();
    conn
}

#[test]
fn test_get_agents_query_empty_db() {
    let conn = setup_test_db();

    let mut stmt = conn
        .prepare(
            "SELECT i.agent_id, i.name, COALESCE(s.persona_name, ''), i.role, i.image_url,
                    (SELECT COUNT(*) FROM skills WHERE agent_id = i.agent_id) as skill_count,
                    (SELECT COUNT(*) FROM agent_sessions WHERE agent_id = i.agent_id) as session_count
             FROM identity i
             LEFT JOIN soul s ON i.agent_id = s.agent_id",
        )
        .unwrap();

    let rows: Vec<(String, String, String, String, Option<String>, i32, i32)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    assert!(rows.is_empty());
}

#[test]
fn test_get_agents_query_with_data() {
    let conn = setup_test_db();

    let identity = opencrab_db::queries::IdentityRow {
        agent_id: "test-agent".to_string(),
        name: "Test Agent".to_string(),
        role: "discussant".to_string(),
        job_title: None,
        organization: None,
        image_url: None,
        metadata_json: None,
    };
    opencrab_db::queries::upsert_identity(&conn, &identity).unwrap();

    let soul = opencrab_db::queries::SoulRow {
        agent_id: "test-agent".to_string(),
        persona_name: "Test Persona".to_string(),
        social_style_json: "{}".to_string(),
        personality_json: r#"{"openness":0.5,"conscientiousness":0.5,"extraversion":0.5,"agreeableness":0.5,"neuroticism":0.0}"#.to_string(),
        thinking_style_json: "{}".to_string(),
        custom_traits_json: None,
    };
    opencrab_db::queries::upsert_soul(&conn, &soul).unwrap();

    let mut stmt = conn
        .prepare(
            "SELECT i.agent_id, i.name, COALESCE(s.persona_name, ''), i.role, i.image_url,
                    (SELECT COUNT(*) FROM skills WHERE agent_id = i.agent_id) as skill_count,
                    (SELECT COUNT(*) FROM agent_sessions WHERE agent_id = i.agent_id) as session_count
             FROM identity i
             LEFT JOIN soul s ON i.agent_id = s.agent_id",
        )
        .unwrap();

    let rows: Vec<(String, String, String, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
            ))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "test-agent");
    assert_eq!(rows[0].1, "Test Agent");
    assert_eq!(rows[0].2, "Test Persona");
    assert_eq!(rows[0].3, "discussant");
}

#[test]
fn test_get_agent_detail() {
    let conn = setup_test_db();

    let identity = opencrab_db::queries::IdentityRow {
        agent_id: "detail-test".to_string(),
        name: "Detail Test".to_string(),
        role: "facilitator".to_string(),
        job_title: Some("Engineer".to_string()),
        organization: Some("OpenCrab".to_string()),
        image_url: None,
        metadata_json: None,
    };
    opencrab_db::queries::upsert_identity(&conn, &identity).unwrap();

    let soul = opencrab_db::queries::SoulRow {
        agent_id: "detail-test".to_string(),
        persona_name: "The Facilitator".to_string(),
        social_style_json: r#"{"style":"warm"}"#.to_string(),
        personality_json: r#"{"openness":0.9,"conscientiousness":0.7,"extraversion":0.8,"agreeableness":0.6,"neuroticism":0.1}"#.to_string(),
        thinking_style_json: r#"{"primary":"Intuitive","secondary":"Creative","description":"Visionary thinker"}"#.to_string(),
        custom_traits_json: None,
    };
    opencrab_db::queries::upsert_soul(&conn, &soul).unwrap();

    // get_identityとget_soulで取得
    let loaded_identity = opencrab_db::queries::get_identity(&conn, "detail-test").unwrap().unwrap();
    assert_eq!(loaded_identity.name, "Detail Test");
    assert_eq!(loaded_identity.role, "facilitator");
    assert_eq!(loaded_identity.job_title, Some("Engineer".to_string()));

    let loaded_soul = opencrab_db::queries::get_soul(&conn, "detail-test").unwrap().unwrap();
    assert_eq!(loaded_soul.persona_name, "The Facilitator");

    // personality JSONがパースできることを確認
    let personality: serde_json::Value = serde_json::from_str(&loaded_soul.personality_json).unwrap();
    assert_eq!(personality["openness"], 0.9);

    // thinking style JSONがパースできることを確認
    let thinking: serde_json::Value = serde_json::from_str(&loaded_soul.thinking_style_json).unwrap();
    assert_eq!(thinking["primary"], "Intuitive");
}

#[test]
fn test_create_and_delete_agent() {
    let conn = setup_test_db();

    let identity = opencrab_db::queries::IdentityRow {
        agent_id: "delete-test".to_string(),
        name: "Delete Me".to_string(),
        role: "observer".to_string(),
        job_title: None,
        organization: None,
        image_url: None,
        metadata_json: None,
    };
    opencrab_db::queries::upsert_identity(&conn, &identity).unwrap();

    let soul = opencrab_db::queries::SoulRow {
        agent_id: "delete-test".to_string(),
        persona_name: "Doomed".to_string(),
        social_style_json: "{}".to_string(),
        personality_json: "{}".to_string(),
        thinking_style_json: "{}".to_string(),
        custom_traits_json: None,
    };
    opencrab_db::queries::upsert_soul(&conn, &soul).unwrap();

    // 存在確認
    assert!(opencrab_db::queries::get_identity(&conn, "delete-test").unwrap().is_some());

    // 削除
    let deleted = opencrab_db::queries::delete_agent(&conn, "delete-test").unwrap();
    assert!(deleted);

    // 削除後の確認
    assert!(opencrab_db::queries::get_identity(&conn, "delete-test").unwrap().is_none());
    assert!(opencrab_db::queries::get_soul(&conn, "delete-test").unwrap().is_none());
}

#[test]
fn test_update_soul() {
    let conn = setup_test_db();

    let identity = opencrab_db::queries::IdentityRow {
        agent_id: "soul-test".to_string(),
        name: "Soul Test".to_string(),
        role: "discussant".to_string(),
        job_title: None,
        organization: None,
        image_url: None,
        metadata_json: None,
    };
    opencrab_db::queries::upsert_identity(&conn, &identity).unwrap();

    // 初期soul
    let soul = opencrab_db::queries::SoulRow {
        agent_id: "soul-test".to_string(),
        persona_name: "Original".to_string(),
        social_style_json: "{}".to_string(),
        personality_json: r#"{"openness":0.5,"conscientiousness":0.5,"extraversion":0.5,"agreeableness":0.5,"neuroticism":0.0}"#.to_string(),
        thinking_style_json: r#"{"primary":"Analytical","secondary":"Practical","description":""}"#.to_string(),
        custom_traits_json: None,
    };
    opencrab_db::queries::upsert_soul(&conn, &soul).unwrap();

    // 更新
    let updated_soul = opencrab_db::queries::SoulRow {
        agent_id: "soul-test".to_string(),
        persona_name: "Updated".to_string(),
        social_style_json: r#"{"style":"assertive"}"#.to_string(),
        personality_json: r#"{"openness":0.9,"conscientiousness":0.8,"extraversion":0.7,"agreeableness":0.6,"neuroticism":0.1}"#.to_string(),
        thinking_style_json: r#"{"primary":"Creative","secondary":"Intuitive","description":"Visionary"}"#.to_string(),
        custom_traits_json: None,
    };
    opencrab_db::queries::upsert_soul(&conn, &updated_soul).unwrap();

    let loaded = opencrab_db::queries::get_soul(&conn, "soul-test").unwrap().unwrap();
    assert_eq!(loaded.persona_name, "Updated");
    assert_eq!(loaded.social_style_json, r#"{"style":"assertive"}"#);

    let personality: serde_json::Value = serde_json::from_str(&loaded.personality_json).unwrap();
    assert_eq!(personality["openness"], 0.9);
}

#[test]
fn test_update_identity() {
    let conn = setup_test_db();

    let identity = opencrab_db::queries::IdentityRow {
        agent_id: "identity-test".to_string(),
        name: "Original Name".to_string(),
        role: "discussant".to_string(),
        job_title: None,
        organization: None,
        image_url: None,
        metadata_json: None,
    };
    opencrab_db::queries::upsert_identity(&conn, &identity).unwrap();

    // 更新
    let updated = opencrab_db::queries::IdentityRow {
        agent_id: "identity-test".to_string(),
        name: "New Name".to_string(),
        role: "facilitator".to_string(),
        job_title: Some("CTO".to_string()),
        organization: Some("Acme Corp".to_string()),
        image_url: None,
        metadata_json: None,
    };
    opencrab_db::queries::upsert_identity(&conn, &updated).unwrap();

    let loaded = opencrab_db::queries::get_identity(&conn, "identity-test").unwrap().unwrap();
    assert_eq!(loaded.name, "New Name");
    assert_eq!(loaded.role, "facilitator");
    assert_eq!(loaded.job_title, Some("CTO".to_string()));
    assert_eq!(loaded.organization, Some("Acme Corp".to_string()));
}

#[test]
fn test_skills_query() {
    let conn = setup_test_db();

    let identity = opencrab_db::queries::IdentityRow {
        agent_id: "skill-agent".to_string(),
        name: "Skill Agent".to_string(),
        role: "discussant".to_string(),
        job_title: None,
        organization: None,
        image_url: None,
        metadata_json: None,
    };
    opencrab_db::queries::upsert_identity(&conn, &identity).unwrap();

    // スキルが空の状態
    let skills = opencrab_db::queries::list_skills(&conn, "skill-agent", false).unwrap();
    assert!(skills.is_empty());

    // スキルの存在確認（list_skillsはactiveフラグでフィルタ可能）
    let skills_all = opencrab_db::queries::list_skills(&conn, "skill-agent", true).unwrap();
    assert!(skills_all.is_empty());
}

#[test]
fn test_sessions_query() {
    let conn = setup_test_db();

    let sessions = opencrab_db::queries::list_sessions(&conn).unwrap();
    assert!(sessions.is_empty());
}

#[test]
fn test_session_participant_count_parsing() {
    // get_sessions()でparticipant_ids_jsonをパースしてcountを得るロジック
    let json = r#"["agent1","agent2","agent3"]"#;
    let participants: Vec<String> = serde_json::from_str(json).unwrap();
    assert_eq!(participants.len(), 3);

    // 空配列
    let json_empty = "[]";
    let participants_empty: Vec<String> = serde_json::from_str(json_empty).unwrap();
    assert_eq!(participants_empty.len(), 0);

    // 不正なJSON→unwrap_or(0)
    let bad_json = "not json";
    let result = serde_json::from_str::<Vec<String>>(bad_json);
    assert!(result.is_err());
}

#[test]
fn test_curated_memories_query() {
    let conn = setup_test_db();

    let memories = opencrab_db::queries::list_curated_memories(&conn, "nonexistent").unwrap();
    assert!(memories.is_empty());
}

#[test]
fn test_search_session_logs_query() {
    let conn = setup_test_db();

    let results = opencrab_db::queries::search_session_logs(&conn, "agent1", "test query", 50).unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_llm_metrics_summary_query() {
    let conn = setup_test_db();

    let since = chrono::Utc::now().to_rfc3339();
    let summary = opencrab_db::queries::get_llm_metrics_summary(&conn, "agent1", &since).unwrap();
    assert_eq!(summary.count, 0);
}

#[test]
fn test_llm_metrics_detail_query() {
    let conn = setup_test_db();

    let since = chrono::Utc::now().to_rfc3339();
    let mut stmt = conn
        .prepare(
            "SELECT provider, model, SUM(total_tokens), SUM(estimated_cost_usd), COUNT(*), AVG(latency_ms)
             FROM llm_usage_metrics
             WHERE agent_id = ?1 AND timestamp >= ?2
             GROUP BY provider, model
             ORDER BY SUM(estimated_cost_usd) DESC",
        )
        .unwrap();

    let rows: Vec<(String, String)> = stmt
        .query_map(rusqlite::params!["agent1", since], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    assert!(rows.is_empty());
}

#[test]
fn test_session_logs_query() {
    let conn = setup_test_db();

    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, log_type, content, speaker_id, created_at
             FROM memory_sessions
             WHERE session_id = ?1
             ORDER BY created_at ASC",
        )
        .unwrap();

    let rows: Vec<i64> = stmt
        .query_map(rusqlite::params!["nonexistent"], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    assert!(rows.is_empty());
}

#[test]
fn test_mentor_instruction_insert() {
    let conn = setup_test_db();

    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: "mentor".to_string(),
        session_id: "test-session".to_string(),
        log_type: "system".to_string(),
        content: "Please focus on the main topic.".to_string(),
        speaker_id: Some("mentor".to_string()),
        turn_number: None,
        metadata_json: None,
    };
    opencrab_db::queries::insert_session_log(&conn, &log).unwrap();

    // 挿入確認
    let mut stmt = conn
        .prepare(
            "SELECT content, speaker_id FROM memory_sessions WHERE session_id = ?1",
        )
        .unwrap();

    let rows: Vec<(String, Option<String>)> = stmt
        .query_map(rusqlite::params!["test-session"], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "Please focus on the main topic.");
    assert_eq!(rows[0].1, Some("mentor".to_string()));
}
