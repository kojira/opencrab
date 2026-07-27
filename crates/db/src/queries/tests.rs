use rusqlite::{params, Connection};

#[allow(unused_imports)]
use super::*;

#[test]
fn insert_session_dual_writes_agent_sessions() {
    let conn = crate::init_memory().unwrap();
    let session = SessionRow {
        id: "sess-dw".to_string(),
        mode: "discord".to_string(),
        theme: "t".to_string(),
        phase: "active".to_string(),
        turn_number: 0,
        status: "active".to_string(),
        participant_ids_json: "[\"agent-x\",\"agent-y\"]".to_string(),
        facilitator_id: None,
        done_count: 0,
        max_turns: None,
        metadata_json: None,
    };
    insert_session(&conn, &session).unwrap();
    assert_eq!(
        list_session_participants(&conn, "sess-dw").unwrap(),
        vec!["agent-x".to_string(), "agent-y".to_string()]
    );
    assert_eq!(count_sessions_for_agent(&conn, "agent-x").unwrap(), 1);
    // JSON 投影も従来どおり保存されている（wire 契約）
    let row = get_session(&conn, "sess-dw").unwrap().unwrap();
    assert_eq!(row.participant_ids_json, "[\"agent-x\",\"agent-y\"]");
}

fn setup() -> Connection {
    crate::init_memory().expect("failed to init in-memory DB")
}

#[test]
fn test_trusted_user_display_name_round_trip() {
    let conn = setup();
    add_trusted_user(
        &conn,
        TRUSTED_PLATFORM_DISCORD,
        "id-1",
        "a1",
        "42",
        "co_agent",
        "owner",
        "2026-01-01",
        "Crab B",
    )
    .unwrap();
    let row = get_trusted_user(&conn, TRUSTED_PLATFORM_DISCORD, "42", "a1").unwrap();
    assert_eq!(row.display_name, "Crab B");
    assert_eq!(row.permission, "co_agent");

    assert!(update_trusted_user_display_name(&conn, "id-1", "Crab B2").unwrap());
    let rows = list_trusted_users(&conn, "a1").unwrap();
    assert_eq!(rows[0].display_name, "Crab B2");

    // v3 以前の行（display_name / platform とも列 DEFAULT）も読み出せる
    conn.execute(
        "INSERT INTO trusted_users (id, user_id, agent_id, permission, created_by, created_at) \
         VALUES ('id-2', '43', 'a1', 'user', 'owner', '2026-01-01')",
        [],
    )
    .unwrap();
    let row = get_trusted_user(&conn, TRUSTED_PLATFORM_DISCORD, "43", "a1").unwrap();
    assert_eq!(row.display_name, "");
    // 列追加前からある行は従来の経路（discord）として生きる（#214）
    assert_eq!(row.platform, TRUSTED_PLATFORM_DISCORD);
}

// ---- 経路（identity platform）で識別子空間が分かれること（#214） ----

/// 1 件登録するテストヘルパ。
fn add_trusted(conn: &Connection, platform: &str, row_id: &str, user_id: &str, agent_id: &str) {
    add_trusted_user(
        conn,
        platform,
        row_id,
        agent_id,
        user_id,
        "user",
        "owner",
        "2026-01-01",
        "",
    )
    .unwrap();
}

/// 同じ識別子でも経路が違えば別扱い（信頼が経路をまたいで引き継がれない）。
#[test]
fn trust_does_not_cross_platforms() {
    let conn = setup();
    // Discord 経路に "42" を登録する。
    add_trusted(&conn, TRUSTED_PLATFORM_DISCORD, "row-d", "42", "a1");
    assert!(is_trusted_user(&conn, TRUSTED_PLATFORM_DISCORD, "42", "a1"));
    // 同じ文字列を web / REST の識別子として名乗っても、その経路では信頼されない。
    assert!(!is_trusted_user(&conn, TRUSTED_PLATFORM_WEB, "42", "a1"));
    assert!(!is_trusted_user(&conn, TRUSTED_PLATFORM_REST, "42", "a1"));
    assert!(get_trusted_user(&conn, TRUSTED_PLATFORM_WEB, "42", "a1").is_none());

    // 逆向きも同じ: web 経路の登録は Discord 経路へ漏れない。
    add_trusted(&conn, TRUSTED_PLATFORM_WEB, "row-w", "dash-user", "a1");
    assert!(is_trusted_user(
        &conn,
        TRUSTED_PLATFORM_WEB,
        "dash-user",
        "a1"
    ));
    assert!(!is_trusted_user(
        &conn,
        TRUSTED_PLATFORM_DISCORD,
        "dash-user",
        "a1"
    ));
}

/// 登録件数の判定も経路で切られている
/// （ある経路に登録があっても、別経路から見れば「0 件」）。
#[test]
fn trusted_user_count_is_scoped_by_platform() {
    let conn = setup();
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_DISCORD, "a1"), 0);

    add_trusted(&conn, TRUSTED_PLATFORM_WEB, "row-w", "dash-user", "a1");
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_WEB, "a1"), 1);
    // web に 1 件あっても Discord から見れば未登録（= owner のみ許可の段が生きる）。
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_DISCORD, "a1"), 0);
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_REST, "a1"), 0);

    add_trusted(&conn, TRUSTED_PLATFORM_DISCORD, "row-d", "42", "a1");
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_DISCORD, "a1"), 1);
    // エージェントでも切れている
    assert_eq!(trusted_user_count(&conn, TRUSTED_PLATFORM_DISCORD, "a2"), 0);
}

/// 互換読み（暫定）: 自経路の行が無ければ従来の `discord` 経路の行も見る。
/// 従来経路そのもの（discord）はフォールバックしない。
#[test]
fn legacy_fallback_reads_discord_rows_until_migration() {
    let conn = setup();
    add_trusted(&conn, TRUSTED_PLATFORM_DISCORD, "row-d", "42", "a1");

    // 自経路の行が無い → 従来経路の行が見える（既存の信頼が一斉に失効しない）。
    let via_web = get_trusted_user_with_legacy_fallback(&conn, TRUSTED_PLATFORM_WEB, "42", "a1")
        .expect("legacy fallback");
    assert_eq!(via_web.platform, TRUSTED_PLATFORM_DISCORD);
    assert!(
        get_trusted_user_with_legacy_fallback(&conn, TRUSTED_PLATFORM_REST, "42", "a1").is_some()
    );

    // 自経路の行があればそれが優先される（フォールバックへ落ちない）。
    add_trusted(&conn, TRUSTED_PLATFORM_WEB, "row-w", "dash-user", "a1");
    let own = get_trusted_user_with_legacy_fallback(&conn, TRUSTED_PLATFORM_WEB, "dash-user", "a1")
        .expect("own platform row");
    assert_eq!(own.platform, TRUSTED_PLATFORM_WEB);

    // Discord 側は逆向きに漏れない（web の行は Discord からは見えないまま）。
    assert!(get_trusted_user_with_legacy_fallback(
        &conn,
        TRUSTED_PLATFORM_DISCORD,
        "dash-user",
        "a1"
    )
    .is_none());

    // 未登録は経路を問わず None（フォールバックが「誰でも通る」にはならない）。
    assert!(
        get_trusted_user_with_legacy_fallback(&conn, TRUSTED_PLATFORM_WEB, "999", "a1").is_none()
    );
}

#[test]
fn test_task_ledger_insert_and_get_active() {
    let conn = setup();
    let id =
        insert_task_ledger(&conn, "a1", "s1", "build feature", Some("tests pass")).expect("insert");

    let task = get_active_task_for_session(&conn, "a1", "s1")
        .expect("query")
        .expect("active task");
    assert_eq!(task.id, id);
    assert_eq!(task.goal, "build feature");
    assert_eq!(task.contract.as_deref(), Some("tests pass"));
    assert_eq!(task.status, "active");

    // 別セッション / 別エージェントからは見えない
    assert!(get_active_task_for_session(&conn, "a1", "s2")
        .unwrap()
        .is_none());
    assert!(get_active_task_for_session(&conn, "a2", "s1")
        .unwrap()
        .is_none());
    assert!(get_task_ledger(&conn, "a2", id).unwrap().is_none());
}

#[test]
fn test_task_ledger_status_update() {
    let conn = setup();
    let id = insert_task_ledger(&conn, "a1", "s1", "g", None).unwrap();

    assert!(update_task_status(&conn, "a1", id, "done").unwrap());
    assert!(get_active_task_for_session(&conn, "a1", "s1")
        .unwrap()
        .is_none());
    let task = get_task_ledger(&conn, "a1", id).unwrap().unwrap();
    assert_eq!(task.status, "done");

    // 未知の id / 他エージェントは Ok(false)
    assert!(!update_task_status(&conn, "a1", 9999, "done").unwrap());
    assert!(!update_task_status(&conn, "a2", id, "abandoned").unwrap());
}

#[test]
fn test_task_ledger_restart_count_increment() {
    let conn = setup();
    let id = insert_task_ledger(&conn, "a1", "s1", "g", None).unwrap();

    // 新規タスクは 0 から始まる
    let task = get_task_ledger(&conn, "a1", id).unwrap().unwrap();
    assert_eq!(task.restart_count, 0);

    assert!(increment_task_restart_count(&conn, "a1", id).unwrap());
    assert!(increment_task_restart_count(&conn, "a1", id).unwrap());
    let task = get_task_ledger(&conn, "a1", id).unwrap().unwrap();
    assert_eq!(task.restart_count, 2);

    // 未知の id / 他エージェントは Ok(false)（カウントは動かない）
    assert!(!increment_task_restart_count(&conn, "a1", 9999).unwrap());
    assert!(!increment_task_restart_count(&conn, "a2", id).unwrap());
    let task = get_task_ledger(&conn, "a1", id).unwrap().unwrap();
    assert_eq!(task.restart_count, 2);
}

#[test]
fn test_task_ledger_update_goal_contract() {
    let conn = setup();
    let id = insert_task_ledger(&conn, "a1", "s1", "old goal", Some("old contract")).unwrap();

    // contract のみ更新 → goal は据え置き
    assert!(update_task_goal_contract(&conn, "a1", id, None, Some("new contract")).unwrap());
    let task = get_task_ledger(&conn, "a1", id).unwrap().unwrap();
    assert_eq!(task.goal, "old goal");
    assert_eq!(task.contract.as_deref(), Some("new contract"));

    // goal のみ更新 → contract は据え置き
    assert!(update_task_goal_contract(&conn, "a1", id, Some("new goal"), None).unwrap());
    let task = get_task_ledger(&conn, "a1", id).unwrap().unwrap();
    assert_eq!(task.goal, "new goal");
    assert_eq!(task.contract.as_deref(), Some("new contract"));
}

#[test]
fn test_task_ledger_second_active_insert_rejected_by_db() {
    let conn = setup();
    insert_task_ledger(&conn, "a1", "s1", "first", None).unwrap();
    // 部分ユニークインデックスにより同一セッションの2件目の active は DB 層で拒否される
    let err = insert_task_ledger(&conn, "a1", "s1", "second", None).unwrap_err();
    assert!(err.to_string().contains("UNIQUE constraint failed"));
    // close 後は再度 open できる
    let first = get_active_task_for_session(&conn, "a1", "s1")
        .unwrap()
        .unwrap();
    assert!(update_task_status(&conn, "a1", first.id, "done").unwrap());
    insert_task_ledger(&conn, "a1", "s1", "second", None).unwrap();
}

#[test]
fn test_task_progress_bumps_ledger_updated_at() {
    let conn = setup();
    let id = insert_task_ledger(&conn, "a1", "s1", "g", None).unwrap();
    let before = get_task_ledger(&conn, "a1", id)
        .unwrap()
        .unwrap()
        .updated_at;
    insert_task_progress(&conn, id, "progress", "step").unwrap();
    let after = get_task_ledger(&conn, "a1", id)
        .unwrap()
        .unwrap()
        .updated_at;
    assert!(after > before, "updated_at must advance on progress append");
}

#[test]
fn test_task_progress_append_count_and_recent() {
    let conn = setup();
    let id = insert_task_ledger(&conn, "a1", "s1", "g", None).unwrap();
    for i in 1..=15 {
        insert_task_progress(&conn, id, "progress", &format!("step {i}")).unwrap();
    }

    assert_eq!(count_task_progress(&conn, id).unwrap(), 15);
    let recent = list_recent_task_progress(&conn, id, 10).unwrap();
    assert_eq!(recent.len(), 10);
    // 直近10件が時系列順（step 6 .. step 15）
    assert_eq!(recent.first().unwrap().content, "step 6");
    assert_eq!(recent.last().unwrap().content, "step 15");
}

#[test]
fn test_task_progress_cascade_delete() {
    let conn = setup();
    // init_memory は configure_connection を通らないため FK を明示的に有効化する
    conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
    let id = insert_task_ledger(&conn, "a1", "s1", "g", None).unwrap();
    insert_task_progress(&conn, id, "progress", "p1").unwrap();

    conn.execute("DELETE FROM task_ledger WHERE id = ?1", params![id])
        .unwrap();
    assert_eq!(count_task_progress(&conn, id).unwrap(), 0);
}

#[test]
fn test_agent_upsert_and_get() {
    let conn = setup();
    let agent = AgentRow {
        agent_id: "agent-1".to_string(),
        name: "Alice".to_string(),
        job_title: Some("Engineer".to_string()),
        organization: Some("OpenCrab Inc.".to_string()),
        image_url: Some("https://example.com/avatar.png".to_string()),
        persona_name: "Crab".to_string(),
        personality: Some(r#"{"hobby":"coding"}"#.to_string()),
        instructions: String::new(),
        heartbeat_instructions: String::new(),
        model: None,
        reasoning_effort: None,
        web_search: None,
        metadata_json: Some(r#"{"lang":"en"}"#.to_string()),
    };

    upsert_agent(&conn, &agent).unwrap();

    let fetched = get_agent(&conn, "agent-1").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.agent_id, "agent-1");
    assert_eq!(fetched.name, "Alice");
    assert_eq!(fetched.persona_name, "Crab");
    assert_eq!(
        fetched.personality,
        Some(r#"{"hobby":"coding"}"#.to_string())
    );
    assert_eq!(fetched.job_title, Some("Engineer".to_string()));
    assert_eq!(
        fetched.image_url,
        Some("https://example.com/avatar.png".to_string())
    );
    assert_eq!(fetched.metadata_json, Some(r#"{"lang":"en"}"#.to_string()));
}

#[test]
fn test_agent_get_nonexistent() {
    let conn = setup();
    let result = get_agent(&conn, "nonexistent-agent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_effective_model_for_agent() {
    let conn = setup();
    let agent = AgentRow {
        agent_id: "a1".to_string(),
        name: "N".to_string(),
        job_title: None,
        organization: None,
        image_url: None,
        persona_name: "p".to_string(),
        personality: None,
        instructions: String::new(),
        heartbeat_instructions: String::new(),
        model: Some("openai:gpt-4o".to_string()),
        reasoning_effort: None,
        web_search: None,
        metadata_json: None,
    };
    upsert_agent(&conn, &agent).unwrap();
    let m = effective_model_for_agent(&conn, "a1", "anthropic:claude").unwrap();
    assert_eq!(m, "openai:gpt-4o");
    let m2 = effective_model_for_agent(&conn, "a1", "anthropic:claude").unwrap();
    assert_eq!(m2, "openai:gpt-4o");

    let agent2 = AgentRow {
        agent_id: "a2".to_string(),
        name: "N2".to_string(),
        job_title: None,
        organization: None,
        image_url: None,
        persona_name: "p".to_string(),
        personality: None,
        instructions: String::new(),
        heartbeat_instructions: String::new(),
        model: None,
        reasoning_effort: None,
        web_search: None,
        metadata_json: None,
    };
    upsert_agent(&conn, &agent2).unwrap();
    let m3 = effective_model_for_agent(&conn, "a2", "global:default").unwrap();
    assert_eq!(m3, "global:default");
}

// 4. test_curated_memory_crud
#[test]
fn test_curated_memory_crud() {
    let conn = setup();

    let mem1 = CuratedMemoryRow {
        id: "mem-1".to_string(),
        agent_id: "agent-1".to_string(),
        category: "facts".to_string(),
        content: "Rust is a systems programming language.".to_string(),
        created_at: String::new(),
    };
    let mem2 = CuratedMemoryRow {
        id: "mem-2".to_string(),
        agent_id: "agent-1".to_string(),
        category: "facts".to_string(),
        content: "Crabs have ten legs.".to_string(),
        created_at: String::new(),
    };

    upsert_curated_memory(&conn, &mem1).unwrap();
    upsert_curated_memory(&conn, &mem2).unwrap();

    let results = get_curated_memories(&conn, "agent-1", "facts").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content, "Crabs have ten legs.");
}

// 5. test_curated_memory_list_all
#[test]
fn test_curated_memory_list_all() {
    let conn = setup();

    let mem1 = CuratedMemoryRow {
        id: "mem-1".to_string(),
        agent_id: "agent-1".to_string(),
        category: "facts".to_string(),
        content: "The sky is blue.".to_string(),
        created_at: String::new(),
    };
    let mem2 = CuratedMemoryRow {
        id: "mem-2".to_string(),
        agent_id: "agent-1".to_string(),
        category: "opinions".to_string(),
        content: "Rust is great.".to_string(),
        created_at: String::new(),
    };

    upsert_curated_memory(&conn, &mem1).unwrap();
    upsert_curated_memory(&conn, &mem2).unwrap();

    let (all, _total) = list_curated_memories(&conn, "agent-1", 10000, 0).unwrap();
    assert_eq!(all.len(), 2);

    let categories: Vec<&str> = all.iter().map(|m| m.category.as_str()).collect();
    assert!(categories.contains(&"facts"));
    assert!(categories.contains(&"opinions"));
}

// 6. test_session_log_insert_and_fts
#[test]
fn test_session_log_insert_and_fts() {
    let conn = setup();

    let log1 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "The weather is sunny today.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(1),
        metadata_json: None,
        created_at: None,
    };
    let log2 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "I enjoy programming in Rust.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(2),
        metadata_json: None,
        created_at: None,
    };
    let log3 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "Crabs live near the ocean.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(3),
        metadata_json: None,
        created_at: None,
    };

    insert_session_log(&conn, &log1).unwrap();
    insert_session_log(&conn, &log2).unwrap();
    insert_session_log(&conn, &log3).unwrap();

    let results = search_session_logs(&conn, "agent-1", "sunny", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("sunny"));
}

// 7. test_fts_multi_word_search
#[test]
fn test_fts_multi_word_search() {
    let conn = setup();

    let log1 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "Quantum computing will revolutionize cryptography.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(1),
        metadata_json: None,
        created_at: None,
    };
    let log2 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "Classical computing is still dominant.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(2),
        metadata_json: None,
        created_at: None,
    };

    insert_session_log(&conn, &log1).unwrap();
    insert_session_log(&conn, &log2).unwrap();

    let results = search_session_logs(&conn, "agent-1", "quantum cryptography", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("Quantum"));
}

// 8. test_fts_no_results
#[test]
fn test_fts_no_results() {
    let conn = setup();

    let log = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "Hello world from the test.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(1),
        metadata_json: None,
        created_at: None,
    };
    insert_session_log(&conn, &log).unwrap();

    let results = search_session_logs(&conn, "agent-1", "nonexistenttermxyz", 10).unwrap();
    assert!(results.is_empty());
}

// 9. test_skills_crud
#[test]
fn test_skills_crud() {
    let conn = setup();

    let skill = SkillRow {
        id: "skill-1".to_string(),
        agent_id: "agent-1".to_string(),
        name: "Summarization".to_string(),
        description: "Summarize long texts concisely.".to_string(),
        situation_pattern: "when asked to summarize".to_string(),
        guidance: "Extract key points and present them briefly.".to_string(),
        source_type: "acquired".to_string(),
        source_context: Some("learned from session-1".to_string()),
        file_path: None,
        effectiveness: None,
        usage_count: 0,
        is_active: true,
        permission: "\"agent\"".to_string(),
        archived: false,
    };

    insert_skill(&conn, &skill).unwrap();

    let skills = list_skills(&conn, "agent-1", true).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id, "skill-1");
    assert_eq!(skills[0].name, "Summarization");
    assert!(skills[0].is_active);
    assert_eq!(skills[0].usage_count, 0);
    assert_eq!(skills[0].source_type, "acquired");
}

// 10. test_skill_usage_increment
#[test]
fn test_skill_usage_increment() {
    let conn = setup();

    let skill = SkillRow {
        id: "skill-1".to_string(),
        agent_id: "agent-1".to_string(),
        name: "Translation".to_string(),
        description: "Translate between languages.".to_string(),
        situation_pattern: "when translation is needed".to_string(),
        guidance: "Use context-aware translation.".to_string(),
        source_type: "acquired".to_string(),
        source_context: None,
        file_path: None,
        effectiveness: None,
        usage_count: 0,
        is_active: true,
        permission: "\"agent\"".to_string(),
        archived: false,
    };

    insert_skill(&conn, &skill).unwrap();
    increment_skill_usage(&conn, "skill-1").unwrap();

    let skills = list_skills(&conn, "agent-1", true).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].usage_count, 1);
}

// 11a. test_find_skill_by_name_any_includes_archived
#[test]
fn test_find_skill_by_name_any_includes_archived() {
    let conn = setup();

    let skill = SkillRow {
        id: "skill-arch-1".to_string(),
        agent_id: "agent-1".to_string(),
        name: "ArchivedSkill".to_string(),
        description: "Some description".to_string(),
        situation_pattern: "".to_string(),
        guidance: "".to_string(),
        source_type: "acquired".to_string(),
        source_context: None,
        file_path: None,
        effectiveness: None,
        usage_count: 0,
        is_active: false,
        permission: "\"agent\"".to_string(),
        archived: true,
    };
    insert_skill(&conn, &skill).unwrap();

    // find_skill_by_name should NOT find archived
    let not_found = find_skill_by_name(&conn, "agent-1", "ArchivedSkill").unwrap();
    assert!(
        not_found.is_none(),
        "find_skill_by_name should not find archived skill"
    );

    // find_skill_by_name_any SHOULD find archived
    let found = find_skill_by_name_any(&conn, "agent-1", "ArchivedSkill").unwrap();
    assert!(
        found.is_some(),
        "find_skill_by_name_any should find archived skill"
    );
    assert_eq!(found.unwrap().archived, true);
}

// 11b. test_update_skill_full_fields
#[test]
fn test_update_skill_full_fields() {
    let conn = setup();

    let skill = SkillRow {
        id: "skill-upd-1".to_string(),
        agent_id: "agent-1".to_string(),
        name: "UpdateMe".to_string(),
        description: "Original description".to_string(),
        situation_pattern: "original pattern".to_string(),
        guidance: "original guidance".to_string(),
        source_type: "acquired".to_string(),
        source_context: None,
        file_path: None,
        effectiveness: None,
        usage_count: 0,
        is_active: true,
        permission: "\"agent\"".to_string(),
        archived: true,
    };
    insert_skill(&conn, &skill).unwrap();

    // Update with new values including archived=false restore
    let mut updated = skill.clone();
    updated.description = "Updated description".to_string();
    updated.guidance = "Updated guidance".to_string();
    updated.archived = false;
    updated.is_active = true;
    update_skill(&conn, &updated).unwrap();

    let found = find_skill_by_name(&conn, "agent-1", "UpdateMe").unwrap();
    assert!(found.is_some(), "should find restored skill");
    let s = found.unwrap();
    assert_eq!(s.description, "Updated description");
    assert_eq!(s.guidance, "Updated guidance");
    assert_eq!(s.archived, false);
    assert_eq!(s.is_active, true);
}

// 11. test_impressions_upsert_and_get
#[test]
fn test_impressions_upsert_and_get() {
    let conn = setup();

    let impression = ImpressionRow {
        id: "imp-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        target_id: "agent-2".to_string(),
        target_name: "Bob".to_string(),
        personality: "thoughtful and calm".to_string(),
        communication_style: "concise".to_string(),
        recent_behavior: "asked good questions".to_string(),
        agreement: "mostly agree".to_string(),
        notes: "potential collaborator".to_string(),
        last_updated_turn: 5,
    };

    upsert_impression(&conn, &impression).unwrap();

    let results = get_impressions(&conn, "agent-1", "session-1").unwrap();
    assert_eq!(results.len(), 1);
    let fetched = &results[0];
    assert_eq!(fetched.id, "imp-1");
    assert_eq!(fetched.target_id, "agent-2");
    assert_eq!(fetched.target_name, "Bob");
    assert_eq!(fetched.personality, "thoughtful and calm");
    assert_eq!(fetched.communication_style, "concise");
    assert_eq!(fetched.recent_behavior, "asked good questions");
    assert_eq!(fetched.agreement, "mostly agree");
    assert_eq!(fetched.notes, "potential collaborator");
    assert_eq!(fetched.last_updated_turn, 5);
}

// 12. test_session_crud
#[test]
fn test_session_crud() {
    let conn = setup();

    let session = SessionRow {
        id: "session-1".to_string(),
        mode: "facilitated".to_string(),
        theme: "AI Ethics Discussion".to_string(),
        phase: "divergent".to_string(),
        turn_number: 0,
        status: "active".to_string(),
        participant_ids_json: r#"["agent-1","agent-2"]"#.to_string(),
        facilitator_id: Some("agent-1".to_string()),
        done_count: 0,
        max_turns: Some(10),
        metadata_json: None,
    };

    insert_session(&conn, &session).unwrap();

    let fetched = get_session(&conn, "session-1").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.id, "session-1");
    assert_eq!(fetched.mode, "facilitated");
    assert_eq!(fetched.theme, "AI Ethics Discussion");
    assert_eq!(fetched.phase, "divergent");
    assert_eq!(fetched.turn_number, 0);
    assert_eq!(fetched.status, "active");
    assert_eq!(fetched.facilitator_id, Some("agent-1".to_string()));
    assert_eq!(fetched.max_turns, Some(10));

    let all = list_sessions(&conn).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, "session-1");
}

// 13. test_llm_metrics_insert_and_summary
#[test]
fn test_llm_metrics_insert_and_summary() {
    let conn = setup();

    let metrics1 = LlmMetricsRow {
        id: "metrics-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("session-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4".to_string(),
        purpose: "discussion".to_string(),
        task_type: Some("chat".to_string()),
        complexity: Some("medium".to_string()),
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        estimated_cost_usd: 0.005,
        latency_ms: 1200,
        time_to_first_token_ms: Some(200),
    };

    let metrics2 = LlmMetricsRow {
        id: "metrics-2".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("session-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4".to_string(),
        purpose: "summarization".to_string(),
        task_type: Some("summary".to_string()),
        complexity: Some("low".to_string()),
        input_tokens: 200,
        output_tokens: 80,
        total_tokens: 280,
        estimated_cost_usd: 0.008,
        latency_ms: 800,
        time_to_first_token_ms: Some(150),
    };

    insert_llm_metrics(&conn, &metrics1).unwrap();
    insert_llm_metrics(&conn, &metrics2).unwrap();

    let summary = get_llm_metrics_summary(&conn, "agent-1", "2020-01-01").unwrap();
    assert_eq!(summary.count, 2);
    assert_eq!(summary.total_tokens, Some(430));
    let total_cost = summary.total_cost.unwrap();
    assert!((total_cost - 0.013).abs() < 1e-9);
    let avg_latency = summary.avg_latency.unwrap();
    assert!((avg_latency - 1000.0).abs() < 1e-9);
}

// 14. test_llm_metrics_evaluation_update
#[test]
fn test_llm_metrics_evaluation_update() {
    let conn = setup();

    let metrics = LlmMetricsRow {
        id: "metrics-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("session-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4".to_string(),
        purpose: "discussion".to_string(),
        task_type: Some("chat".to_string()),
        complexity: Some("medium".to_string()),
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        estimated_cost_usd: 0.005,
        latency_ms: 1200,
        time_to_first_token_ms: Some(200),
    };

    insert_llm_metrics(&conn, &metrics).unwrap();
    update_llm_metrics_evaluation(&conn, "metrics-1", 0.95, true, "excellent response").unwrap();

    // Read back via raw SQL to verify the evaluation columns
    let (quality_score, task_success, self_evaluation): (f64, i32, String) = conn
        .query_row(
            "SELECT quality_score, task_success, self_evaluation FROM llm_usage_metrics WHERE id = ?1",
            params!["metrics-1"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    assert!((quality_score - 0.95).abs() < 1e-9);
    assert_eq!(task_success, 1);
    assert_eq!(self_evaluation, "excellent response");
}

// 14b. test_llm_metrics_by_model
#[test]
fn test_llm_metrics_by_model() {
    let conn = setup();

    let m1 = LlmMetricsRow {
        id: "m-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("s-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        purpose: "conversation".to_string(),
        task_type: Some("chat".to_string()),
        complexity: Some("medium".to_string()),
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        estimated_cost_usd: 0.005,
        latency_ms: 1200,
        time_to_first_token_ms: Some(200),
    };
    let m2 = LlmMetricsRow {
        id: "m-2".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("s-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        purpose: "conversation".to_string(),
        task_type: Some("chat".to_string()),
        complexity: Some("low".to_string()),
        input_tokens: 80,
        output_tokens: 40,
        total_tokens: 120,
        estimated_cost_usd: 0.001,
        latency_ms: 400,
        time_to_first_token_ms: Some(100),
    };
    let m3 = LlmMetricsRow {
        id: "m-3".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("s-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        purpose: "analysis".to_string(),
        task_type: Some("summary".to_string()),
        complexity: Some("low".to_string()),
        input_tokens: 60,
        output_tokens: 30,
        total_tokens: 90,
        estimated_cost_usd: 0.0008,
        latency_ms: 300,
        time_to_first_token_ms: Some(80),
    };

    insert_llm_metrics(&conn, &m1).unwrap();
    insert_llm_metrics(&conn, &m2).unwrap();
    insert_llm_metrics(&conn, &m3).unwrap();

    let stats = get_llm_metrics_by_model(&conn, "agent-1", "2020-01-01").unwrap();
    assert_eq!(stats.len(), 2);

    // gpt-4o-mini has 2 records, gpt-4o has 1 → sorted by count DESC
    assert_eq!(stats[0].model, "gpt-4o-mini");
    assert_eq!(stats[0].count, 2);
    assert_eq!(stats[0].total_tokens, 210);
    assert!((stats[0].total_cost - 0.0018).abs() < 1e-9);

    assert_eq!(stats[1].model, "gpt-4o");
    assert_eq!(stats[1].count, 1);
}

// 14c. test_llm_metrics_by_model_and_purpose
#[test]
fn test_llm_metrics_by_model_and_purpose() {
    let conn = setup();

    // gpt-4o for conversation
    let m1 = LlmMetricsRow {
        id: "mp-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("s-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        purpose: "conversation".to_string(),
        task_type: Some("chat".to_string()),
        complexity: None,
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        estimated_cost_usd: 0.005,
        latency_ms: 2000,
        time_to_first_token_ms: None,
    };
    // gpt-4o for analysis
    let m2 = LlmMetricsRow {
        id: "mp-2".to_string(),
        purpose: "analysis".to_string(),
        estimated_cost_usd: 0.008,
        latency_ms: 3000,
        ..m1.clone()
    };
    // gpt-4o-mini for conversation
    let m3 = LlmMetricsRow {
        id: "mp-3".to_string(),
        model: "gpt-4o-mini".to_string(),
        purpose: "conversation".to_string(),
        estimated_cost_usd: 0.001,
        latency_ms: 400,
        ..m1.clone()
    };
    // gpt-4o-mini for analysis
    let m4 = LlmMetricsRow {
        id: "mp-4".to_string(),
        model: "gpt-4o-mini".to_string(),
        purpose: "analysis".to_string(),
        estimated_cost_usd: 0.0015,
        latency_ms: 500,
        ..m1.clone()
    };

    insert_llm_metrics(&conn, &m1).unwrap();
    insert_llm_metrics(&conn, &m2).unwrap();
    insert_llm_metrics(&conn, &m3).unwrap();
    insert_llm_metrics(&conn, &m4).unwrap();

    let stats = get_llm_metrics_by_model_and_purpose(&conn, "agent-1", "2020-01-01").unwrap();
    // Should have 4 entries: (gpt-4o, analysis), (gpt-4o, conversation), (gpt-4o-mini, analysis), (gpt-4o-mini, conversation)
    assert_eq!(stats.len(), 4);

    // Verify each entry has correct purpose.
    let purposes: Vec<&str> = stats.iter().map(|s| s.purpose.as_str()).collect();
    assert!(purposes.contains(&"conversation"));
    assert!(purposes.contains(&"analysis"));

    // Verify we can distinguish same model in different purposes.
    let gpt4o_conv = stats
        .iter()
        .find(|s| s.model == "gpt-4o" && s.purpose == "conversation")
        .unwrap();
    let gpt4o_anl = stats
        .iter()
        .find(|s| s.model == "gpt-4o" && s.purpose == "analysis")
        .unwrap();
    assert!((gpt4o_conv.total_cost - 0.005).abs() < 1e-9);
    assert!((gpt4o_anl.total_cost - 0.008).abs() < 1e-9);
}

// 15. test_model_pricing_upsert_and_get
#[test]
fn test_model_pricing_upsert_and_get() {
    let conn = setup();

    let pricing = ModelPricingRow {
        provider: "openai".to_string(),
        model: "gpt-4".to_string(),
        input_price_per_1m: 30.0,
        output_price_per_1m: 60.0,
        context_window: Some(128000),
    };

    upsert_model_pricing(&conn, &pricing).unwrap();

    let fetched = get_model_pricing(&conn, "openai", "gpt-4").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.provider, "openai");
    assert_eq!(fetched.model, "gpt-4");
    assert!((fetched.input_price_per_1m - 30.0).abs() < 1e-9);
    assert!((fetched.output_price_per_1m - 60.0).abs() < 1e-9);
    assert_eq!(fetched.context_window, Some(128000));
}

// 16. test_heartbeat_log_insert
#[test]
fn test_heartbeat_log_insert() {
    let conn = setup();

    let result = insert_heartbeat_log(&conn, "agent-1", "idle", Some(r#"{"action":"none"}"#));
    assert!(result.is_ok());
}

// ── delete_agent ──

#[test]
fn test_delete_agent() {
    let conn = setup();

    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: "del-1".into(),
            name: "DeleteMe".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "Doomed".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    upsert_curated_memory(
        &conn,
        &CuratedMemoryRow {
            id: "cm-del-1".into(),
            agent_id: "del-1".into(),
            category: "fact".into(),
            content: "will be deleted".into(),
            created_at: String::new(),
        },
    )
    .unwrap();

    assert!(get_agent(&conn, "del-1").unwrap().is_some());

    let deleted = delete_agent(&conn, "del-1").unwrap();
    assert!(deleted);

    assert!(get_agent(&conn, "del-1").unwrap().is_none());
    assert!(list_curated_memories(&conn, "del-1", 10000, 0)
        .unwrap()
        .0
        .is_empty());
}

#[test]
fn test_delete_agent_nonexistent() {
    let conn = setup();
    let deleted = delete_agent(&conn, "no-such-agent").unwrap();
    assert!(!deleted);
}

// ── find_agents ──

#[test]
fn test_find_agents_by_id_prefix() {
    let conn = setup();
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: "abc-12345".into(),
            name: "Alice".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "a".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: "xyz-99999".into(),
            name: "Bob".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "b".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();

    // Search by ID prefix
    let results = find_agents(&conn, "abc").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, "Alice");

    // Search by name
    let results = find_agents(&conn, "bob").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, "Bob");

    // No match
    let results = find_agents(&conn, "zzz").unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_find_agents_partial_name() {
    let conn = setup();
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: "agent-find-1".into(),
            name: "Creative Researcher".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "cr".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();

    let results = find_agents(&conn, "creative").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, "Creative Researcher");

    let results = find_agents(&conn, "researcher").unwrap();
    assert_eq!(results.len(), 1);
}

// ── Agent CRUD full cycle ──

#[test]
fn test_agent_crud_full_cycle() {
    let conn = setup();

    let agent_id = "crud-agent-1";
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: agent_id.into(),
            name: "TestAgent".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "Original Persona".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();

    let row = get_agent(&conn, agent_id).unwrap().unwrap();
    assert_eq!(row.name, "TestAgent");
    assert_eq!(row.persona_name, "Original Persona");

    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: agent_id.into(),
            name: "UpdatedAgent".into(),
            job_title: Some("Lead".into()),
            organization: None,
            image_url: None,
            persona_name: "Updated Persona".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();

    let row = get_agent(&conn, agent_id).unwrap().unwrap();
    assert_eq!(row.name, "UpdatedAgent");
    assert_eq!(row.job_title, Some("Lead".to_string()));
    assert_eq!(row.persona_name, "Updated Persona");

    // Find
    let results = find_agents(&conn, "Updated").unwrap();
    assert_eq!(results.len(), 1);

    // Delete
    let deleted = delete_agent(&conn, agent_id).unwrap();
    assert!(deleted);
    assert!(get_agent(&conn, agent_id).unwrap().is_none());

    // Find after delete
    let results = find_agents(&conn, "Updated").unwrap();
    assert!(results.is_empty());
}

// ── Discord Channel Config ──

#[test]
fn test_channel_config_upsert_and_get() {
    let conn = setup();

    let cfg = ChannelConfigRow {
        channel_id: "123456".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "general".to_string(),
        readable: true,
        writable: false,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };

    upsert_channel_config(&conn, &cfg).unwrap();

    let fetched = get_channel_config(&conn, "123456").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.channel_id, "123456");
    assert_eq!(fetched.guild_id, "guild-1");
    assert_eq!(fetched.channel_name, "general");
    assert!(fetched.readable);
    assert!(!fetched.writable);
}

#[test]
fn test_channel_config_upsert_update() {
    let conn = setup();

    let cfg = ChannelConfigRow {
        channel_id: "123456".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "general".to_string(),
        readable: true,
        writable: true,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };
    upsert_channel_config(&conn, &cfg).unwrap();

    // Update writable to false
    let cfg2 = ChannelConfigRow {
        writable: false,
        ..cfg
    };
    upsert_channel_config(&conn, &cfg2).unwrap();

    let fetched = get_channel_config(&conn, "123456").unwrap().unwrap();
    assert!(fetched.readable);
    assert!(!fetched.writable);
}

#[test]
fn test_channel_config_list_by_guild() {
    let conn = setup();

    let cfg1 = ChannelConfigRow {
        channel_id: "ch-1".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "general".to_string(),
        readable: true,
        writable: true,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };
    let cfg2 = ChannelConfigRow {
        channel_id: "ch-2".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "random".to_string(),
        readable: false,
        writable: true,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };
    let cfg3 = ChannelConfigRow {
        channel_id: "ch-3".to_string(),
        agent_id: String::new(),
        guild_id: "guild-2".to_string(),
        channel_name: "other".to_string(),
        readable: true,
        writable: true,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };

    upsert_channel_config(&conn, &cfg1).unwrap();
    upsert_channel_config(&conn, &cfg2).unwrap();
    upsert_channel_config(&conn, &cfg3).unwrap();

    let results = list_channel_configs_by_guild(&conn, "guild-1").unwrap();
    assert_eq!(results.len(), 2);

    let results2 = list_channel_configs_by_guild(&conn, "guild-2").unwrap();
    assert_eq!(results2.len(), 1);
}

#[test]
fn test_is_channel_readable_writable_defaults() {
    let conn = setup();

    // No config → defaults to true
    assert!(is_channel_readable(&conn, "unknown-ch"));
    assert!(is_channel_writable(&conn, "unknown-ch"));

    // Set readable=false
    let cfg = ChannelConfigRow {
        channel_id: "ch-blocked".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "blocked".to_string(),
        readable: false,
        writable: false,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };
    upsert_channel_config(&conn, &cfg).unwrap();

    assert!(!is_channel_readable(&conn, "ch-blocked"));
    assert!(!is_channel_writable(&conn, "ch-blocked"));
}

// ── Heartbeat Instructions ──

fn hb_agent(id: &str, heartbeat: &str) -> AgentRow {
    AgentRow {
        agent_id: id.to_string(),
        name: "N".to_string(),
        job_title: None,
        organization: None,
        image_url: None,
        persona_name: "P".to_string(),
        personality: None,
        instructions: String::new(),
        heartbeat_instructions: heartbeat.to_string(),
        model: None,
        reasoning_effort: None,
        web_search: None,
        metadata_json: None,
    }
}

fn hb_channel(channel_id: &str, agent_id: &str, heartbeat: &str) -> ChannelConfigRow {
    ChannelConfigRow {
        channel_id: channel_id.to_string(),
        agent_id: agent_id.to_string(),
        guild_id: "g1".to_string(),
        channel_name: String::new(),
        readable: true,
        writable: true,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: heartbeat.to_string(),
    }
}

/// T-1.1 / T-1.2: agents.heartbeat_instructions round-trips and patches independently.
#[test]
fn test_agent_heartbeat_instructions_roundtrip_and_patch() {
    let conn = setup();
    upsert_agent(&conn, &hb_agent("a1", "話題があるときだけ話す")).unwrap();
    let got = get_agent(&conn, "a1").unwrap().unwrap();
    assert_eq!(got.heartbeat_instructions, "話題があるときだけ話す");
    assert_eq!(got.instructions, "");

    // patch only heartbeat_instructions; other fields stay.
    let patch = AgentPatch {
        heartbeat_instructions: Some("業務連絡のみ".to_string()),
        ..Default::default()
    };
    assert!(apply_agent_patch(&conn, "a1", &patch).unwrap());
    let got = get_agent(&conn, "a1").unwrap().unwrap();
    assert_eq!(got.heartbeat_instructions, "業務連絡のみ");
    assert_eq!(got.name, "N");
    assert_eq!(got.persona_name, "P");
}

/// T-1.3: channel override round-trips.
#[test]
fn test_channel_heartbeat_instructions_roundtrip() {
    let conn = setup();
    upsert_channel_config(&conn, &hb_channel("ch1", "a1", "雑談禁止")).unwrap();
    let got = get_channel_config_for_agent(&conn, "ch1", "a1")
        .unwrap()
        .unwrap();
    assert_eq!(got.heartbeat_instructions, "雑談禁止");
}

/// T-2.1: priority channel(agent) > channel(global) > agent global.
#[test]
fn test_resolve_priority() {
    let conn = setup();
    upsert_agent(&conn, &hb_agent("a1", "AGENT")).unwrap();
    upsert_channel_config(&conn, &hb_channel("ch1", "", "GLOBAL_CH")).unwrap();
    upsert_channel_config(&conn, &hb_channel("ch1", "a1", "AGENT_CH")).unwrap();

    // channel(agent) wins and is concatenated after agent global.
    let r = resolve_heartbeat_instructions(&conn, "a1", "ch1");
    assert_eq!(r.source, "agent+channel");
    assert_eq!(r.text, "AGENT\n\nAGENT_CH");

    // remove channel(agent) override → falls back to channel(global).
    delete_channel_config_for_agent(&conn, "ch1", "a1").unwrap();
    let r = resolve_heartbeat_instructions(&conn, "a1", "ch1");
    assert_eq!(r.text, "AGENT\n\nGLOBAL_CH");

    // remove channel(global) → agent global only.
    delete_channel_config_for_agent(&conn, "ch1", "").unwrap();
    let r = resolve_heartbeat_instructions(&conn, "a1", "ch1");
    assert_eq!(r.source, "agent");
    assert_eq!(r.text, "AGENT");
}

/// T-2.2: all empty → default fallback.
#[test]
fn test_resolve_default_fallback() {
    let conn = setup();
    upsert_agent(&conn, &hb_agent("a1", "")).unwrap();
    let r = resolve_heartbeat_instructions(&conn, "a1", "ch-none");
    assert_eq!(r.source, "default");
    assert_eq!(r.text, DEFAULT_HEARTBEAT_INSTRUCTIONS);
}

/// T-2.4: clamp to max length and strip control characters.
#[test]
fn test_sanitize_clamp_and_control_chars() {
    let long = "あ".repeat(MAX_HEARTBEAT_INSTRUCTIONS_LEN + 100);
    let out = sanitize_heartbeat_instructions(&long);
    assert_eq!(out.chars().count(), MAX_HEARTBEAT_INSTRUCTIONS_LEN);

    let dirty = "ok\u{0007}line\nnext\ttab";
    let cleaned = sanitize_heartbeat_instructions(dirty);
    assert!(!cleaned.contains('\u{0007}'));
    assert!(cleaned.contains('\n'));
    assert!(cleaned.contains('\t'));
    assert_eq!(cleaned, "okline\nnext\ttab");
}

/// T-3.2: audit row records old/new/reason and is retrievable.
#[test]
fn test_heartbeat_instructions_audit_roundtrip() {
    let conn = setup();
    let audit = HeartbeatInstructionsAuditRow {
        agent_id: "a1".to_string(),
        scope: "agent".to_string(),
        channel_id: None,
        caller_identity: "owner".to_string(),
        caller_discord_id: Some("123".to_string()),
        old_value: Some("old".to_string()),
        new_value: Some("new".to_string()),
        reason: Some("オーナー依頼".to_string()),
    };
    insert_heartbeat_instructions_audit(&conn, &audit).unwrap();
    let rows = list_heartbeat_instructions_audit(&conn, "a1", 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].old_value.as_deref(), Some("old"));
    assert_eq!(rows[0].new_value.as_deref(), Some("new"));
    assert_eq!(rows[0].reason.as_deref(), Some("オーナー依頼"));
}

// ── Agent Discord Config ──

#[test]
fn test_agent_discord_config_upsert_and_get() {
    let conn = setup();

    let cfg = AgentDiscordConfigRow {
        agent_id: "agent-1".to_string(),
        bot_token: "TOKEN_ABC_12345".to_string(),
        owner_discord_id: "390123456789".to_string(),
        enabled: true,
    };

    upsert_agent_discord_config(&conn, &cfg).unwrap();

    let fetched = get_agent_discord_config(&conn, "agent-1").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.agent_id, "agent-1");
    assert_eq!(fetched.bot_token, "TOKEN_ABC_12345");
    assert_eq!(fetched.owner_discord_id, "390123456789");
    assert!(fetched.enabled);
}

#[test]
fn test_agent_discord_config_get_nonexistent() {
    let conn = setup();
    let result = get_agent_discord_config(&conn, "no-such-agent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_agent_discord_config_upsert_update() {
    let conn = setup();

    let cfg = AgentDiscordConfigRow {
        agent_id: "agent-1".to_string(),
        bot_token: "OLD_TOKEN".to_string(),
        owner_discord_id: "".to_string(),
        enabled: true,
    };
    upsert_agent_discord_config(&conn, &cfg).unwrap();

    // Update token and owner
    let cfg2 = AgentDiscordConfigRow {
        agent_id: "agent-1".to_string(),
        bot_token: "NEW_TOKEN".to_string(),
        owner_discord_id: "999888777".to_string(),
        enabled: false,
    };
    upsert_agent_discord_config(&conn, &cfg2).unwrap();

    let fetched = get_agent_discord_config(&conn, "agent-1").unwrap().unwrap();
    assert_eq!(fetched.bot_token, "NEW_TOKEN");
    assert_eq!(fetched.owner_discord_id, "999888777");
    assert!(!fetched.enabled);
}

#[test]
fn test_agent_discord_config_delete() {
    let conn = setup();

    let cfg = AgentDiscordConfigRow {
        agent_id: "agent-del".to_string(),
        bot_token: "TOKEN".to_string(),
        owner_discord_id: "".to_string(),
        enabled: true,
    };
    upsert_agent_discord_config(&conn, &cfg).unwrap();
    assert!(get_agent_discord_config(&conn, "agent-del")
        .unwrap()
        .is_some());

    let deleted = delete_agent_discord_config(&conn, "agent-del").unwrap();
    assert!(deleted);
    assert!(get_agent_discord_config(&conn, "agent-del")
        .unwrap()
        .is_none());

    // Delete nonexistent → false
    let deleted2 = delete_agent_discord_config(&conn, "agent-del").unwrap();
    assert!(!deleted2);
}

#[test]
fn test_list_enabled_agent_discord_configs() {
    let conn = setup();

    let cfg1 = AgentDiscordConfigRow {
        agent_id: "a1".to_string(),
        bot_token: "T1".to_string(),
        owner_discord_id: "".to_string(),
        enabled: true,
    };
    let cfg2 = AgentDiscordConfigRow {
        agent_id: "a2".to_string(),
        bot_token: "T2".to_string(),
        owner_discord_id: "".to_string(),
        enabled: false, // disabled
    };
    let cfg3 = AgentDiscordConfigRow {
        agent_id: "a3".to_string(),
        bot_token: "T3".to_string(),
        owner_discord_id: "owner".to_string(),
        enabled: true,
    };

    upsert_agent_discord_config(&conn, &cfg1).unwrap();
    upsert_agent_discord_config(&conn, &cfg2).unwrap();
    upsert_agent_discord_config(&conn, &cfg3).unwrap();

    let enabled = list_enabled_agent_discord_configs(&conn).unwrap();
    assert_eq!(enabled.len(), 2);

    let ids: Vec<&str> = enabled.iter().map(|c| c.agent_id.as_str()).collect();
    assert!(ids.contains(&"a1"));
    assert!(ids.contains(&"a3"));
    assert!(!ids.contains(&"a2"));
}

#[test]
fn test_set_agent_discord_config_enabled() {
    let conn = setup();

    let cfg = AgentDiscordConfigRow {
        agent_id: "agent-toggle".to_string(),
        bot_token: "TOKEN".to_string(),
        owner_discord_id: "".to_string(),
        enabled: true,
    };
    upsert_agent_discord_config(&conn, &cfg).unwrap();

    // Initially enabled
    let fetched = get_agent_discord_config(&conn, "agent-toggle")
        .unwrap()
        .unwrap();
    assert!(fetched.enabled);

    // Disable
    let updated = set_agent_discord_config_enabled(&conn, "agent-toggle", false).unwrap();
    assert!(updated);
    let fetched = get_agent_discord_config(&conn, "agent-toggle")
        .unwrap()
        .unwrap();
    assert!(!fetched.enabled);

    // Re-enable
    let updated = set_agent_discord_config_enabled(&conn, "agent-toggle", true).unwrap();
    assert!(updated);
    let fetched = get_agent_discord_config(&conn, "agent-toggle")
        .unwrap()
        .unwrap();
    assert!(fetched.enabled);

    // Nonexistent agent → false
    let updated = set_agent_discord_config_enabled(&conn, "no-such", false).unwrap();
    assert!(!updated);
}

#[test]
fn test_delete_agent_also_removes_discord_config() {
    let conn = setup();

    let agent_id = "agent-discord-del";
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: agent_id.into(),
            name: "DiscordAgent".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "d".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    upsert_agent_discord_config(
        &conn,
        &AgentDiscordConfigRow {
            agent_id: agent_id.into(),
            bot_token: "BOT_TOKEN_123".into(),
            owner_discord_id: "owner-1".into(),
            enabled: true,
        },
    )
    .unwrap();

    // Verify exists
    assert!(get_agent_discord_config(&conn, agent_id).unwrap().is_some());

    // Delete agent
    let deleted = delete_agent(&conn, agent_id).unwrap();
    assert!(deleted);

    // Discord config should also be gone
    assert!(get_agent_discord_config(&conn, agent_id).unwrap().is_none());
}

// ============================================
// Agent Webhook Config tests
// ============================================

fn sample_webhook_row(agent_id: &str) -> AgentWebhookConfigRow {
    AgentWebhookConfigRow {
        scope: "agent".into(),
        agent_id: agent_id.into(),
        tool_name: "".into(),
        kind: "subtask".into(),
        url: "https://example.com/hook".into(),
        events_json: Some(r#"["start","done"]"#.into()),
        enabled: true,
        name: Some("default hook".into()),
        created_by: Some("tester".into()),
        output_mode: "full".into(),
        max_chars: 2000,
        updated_at: String::new(),
    }
}

#[test]
fn test_agent_webhook_upsert_and_get_roundtrip() {
    let conn = setup();
    let row = sample_webhook_row("agent-1");
    upsert_agent_webhook_config(&conn, &row).unwrap();

    let fetched = get_agent_webhook_config(&conn, "agent", "agent-1", "", "subtask")
        .unwrap()
        .unwrap();
    assert_eq!(fetched.scope, "agent");
    assert_eq!(fetched.agent_id, "agent-1");
    assert_eq!(fetched.tool_name, "");
    assert_eq!(fetched.kind, "subtask");
    assert_eq!(fetched.url, "https://example.com/hook");
    assert_eq!(fetched.events_json, Some(r#"["start","done"]"#.to_string()));
    assert!(fetched.enabled);
    assert_eq!(fetched.name, Some("default hook".to_string()));
    assert_eq!(fetched.created_by, Some("tester".to_string()));
    assert_eq!(fetched.output_mode, "full");
    assert_eq!(fetched.max_chars, 2000);
    assert!(!fetched.updated_at.is_empty());
}

#[test]
fn test_agent_webhook_get_missing_returns_none() {
    let conn = setup();
    let result = get_agent_webhook_config(&conn, "agent", "nope", "", "subtask").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_agent_webhook_upsert_updates_not_duplicates() {
    let conn = setup();
    let mut row = sample_webhook_row("agent-1");
    upsert_agent_webhook_config(&conn, &row).unwrap();

    row.url = "https://example.com/updated".into();
    upsert_agent_webhook_config(&conn, &row).unwrap();

    let fetched = get_agent_webhook_config(&conn, "agent", "agent-1", "", "subtask")
        .unwrap()
        .unwrap();
    assert_eq!(fetched.url, "https://example.com/updated");

    // Only one row for this PK
    let all = list_agent_webhook_config(&conn, Some("agent-1"), true).unwrap();
    let count = all
        .iter()
        .filter(|r| {
            r.scope == "agent"
                && r.agent_id == "agent-1"
                && r.tool_name.is_empty()
                && r.kind == "subtask"
        })
        .count();
    assert_eq!(count, 1);
}

#[test]
fn test_agent_webhook_list_include_disabled_filter() {
    let conn = setup();
    let mut enabled_row = sample_webhook_row("agent-1");
    enabled_row.kind = "subtask".into();
    upsert_agent_webhook_config(&conn, &enabled_row).unwrap();

    let mut disabled_row = sample_webhook_row("agent-1");
    disabled_row.kind = "tool".into();
    disabled_row.enabled = false;
    upsert_agent_webhook_config(&conn, &disabled_row).unwrap();

    let only_enabled = list_agent_webhook_config(&conn, Some("agent-1"), false).unwrap();
    assert_eq!(only_enabled.len(), 1);
    assert_eq!(only_enabled[0].kind, "subtask");

    let with_disabled = list_agent_webhook_config(&conn, Some("agent-1"), true).unwrap();
    assert_eq!(with_disabled.len(), 2);
}

#[test]
fn test_agent_webhook_list_agent_includes_global() {
    let conn = setup();
    upsert_agent_webhook_config(&conn, &sample_webhook_row("agent-1")).unwrap();

    let mut global = sample_webhook_row("*");
    global.scope = "global".into();
    upsert_agent_webhook_config(&conn, &global).unwrap();

    upsert_agent_webhook_config(&conn, &sample_webhook_row("agent-2")).unwrap();

    let rows = list_agent_webhook_config(&conn, Some("agent-1"), true).unwrap();
    let agent_ids: Vec<&str> = rows.iter().map(|r| r.agent_id.as_str()).collect();
    assert!(agent_ids.contains(&"agent-1"));
    assert!(agent_ids.contains(&"*"));
    assert!(!agent_ids.contains(&"agent-2"));
    assert_eq!(rows.len(), 2);

    // None -> all rows
    let all = list_agent_webhook_config(&conn, None, true).unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn test_agent_webhook_distinct_pk_combos_coexist() {
    let conn = setup();

    let mut r1 = sample_webhook_row("agent-1");
    r1.kind = "subtask".into();
    let mut r2 = sample_webhook_row("agent-1");
    r2.kind = "tool".into();
    r2.tool_name = "my_tool".into();
    let mut r3 = sample_webhook_row("agent-1");
    r3.scope = "tool".into();
    r3.kind = "lifecycle".into();

    upsert_agent_webhook_config(&conn, &r1).unwrap();
    upsert_agent_webhook_config(&conn, &r2).unwrap();
    upsert_agent_webhook_config(&conn, &r3).unwrap();

    let rows = list_agent_webhook_config(&conn, Some("agent-1"), true).unwrap();
    assert_eq!(rows.len(), 3);

    assert!(
        get_agent_webhook_config(&conn, "agent", "agent-1", "", "subtask")
            .unwrap()
            .is_some()
    );
    assert!(
        get_agent_webhook_config(&conn, "agent", "agent-1", "my_tool", "tool")
            .unwrap()
            .is_some()
    );
    assert!(
        get_agent_webhook_config(&conn, "tool", "agent-1", "", "lifecycle")
            .unwrap()
            .is_some()
    );
}

// ============================================
// short_id tests (T-1.1 ~ T-1.6)
// ============================================

#[test]
fn test_next_short_id_empty_table() {
    // T-1.1: Empty table should return "t1"
    let conn = setup();
    let result = next_short_id(&conn, "a1", "t").unwrap();
    assert_eq!(result, "t1");
}

#[test]
fn test_next_short_id_sequential() {
    // T-1.2: With t1,t2,t3 existing, should return "t4"
    let conn = setup();
    for i in 1..=3 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("node-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: format!("Topic {i}"),
                summary: "test".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("t{i}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let result = next_short_id(&conn, "a1", "t").unwrap();
    assert_eq!(result, "t4");
}

#[test]
fn test_next_short_id_independent_prefix() {
    // T-1.3: t1, t2, h1 exist -> prefix="h" returns "h2"
    let conn = setup();
    for (id, prefix, num) in &[("n1", "t", 1), ("n2", "t", 2), ("n3", "h", 1)] {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("{prefix}{num}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let result = next_short_id(&conn, "a1", "h").unwrap();
    assert_eq!(result, "h2");
}

#[test]
fn test_next_short_id_independent_agent() {
    // T-1.4: agent a1 has t1-t10, agent a2 has t1 -> a2 prefix="t" returns "t2"
    let conn = setup();
    for i in 1..=10 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("a1-node-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("t{i}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    insert_index_node(
        &conn,
        &IndexNodeRow {
            id: "a2-node-1".to_string(),
            agent_id: "a2".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: String::new(),
            title: "T".to_string(),
            summary: "s".to_string(),
            start_log_id: None,
            end_log_id: None,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: Some("t1".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
    let result = next_short_id(&conn, "a2", "t").unwrap();
    assert_eq!(result, "t2");
}

#[test]
fn test_next_short_id_with_gaps() {
    // T-1.5: t1, t3, t5 exist (gaps) -> returns "t6" (MAX+1)
    let conn = setup();
    for (id, num) in &[("n1", 1), ("n2", 3), ("n3", 5)] {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("t{num}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let result = next_short_id(&conn, "a1", "t").unwrap();
    assert_eq!(result, "t6");
}

#[test]
fn test_next_short_id_all_prefixes() {
    // T-1.6: All prefix patterns return "{prefix}1" on empty table
    let conn = setup();
    for prefix in &["t", "h", "d", "w", "m", "y", "p", "r", "s"] {
        let result = next_short_id(&conn, "a1", prefix).unwrap();
        assert_eq!(result, format!("{prefix}1"), "Failed for prefix {prefix}");
    }
}

// ============================================
// backfill_short_ids tests (T-1.7 ~ T-1.9)
// ============================================

#[test]
fn test_backfill_short_ids_basic() {
    // T-1.7: 5 topics + 3 dailies with NULL short_id -> get assigned
    let conn = setup();
    for i in 1..=5 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("topic-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: format!("Topic {i}"),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: format!("2026-01-01T00:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: None,
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    for i in 1..=3 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("daily-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "daily".to_string(),
                source_type: String::new(),
                title: format!("Daily {i}"),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: format!("2026-01-01T01:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: None,
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let count = backfill_short_ids(&conn).unwrap();
    assert_eq!(count, 8);
    // Verify topics got t1-t5, dailies got d1-d3
    let node = get_index_node(&conn, "topic-1").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t1".to_string()));
    let node = get_index_node(&conn, "topic-5").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t5".to_string()));
    let node = get_index_node(&conn, "daily-1").unwrap().unwrap();
    assert_eq!(node.short_id, Some("d1".to_string()));
    let node = get_index_node(&conn, "daily-3").unwrap().unwrap();
    assert_eq!(node.short_id, Some("d3".to_string()));
}

#[test]
fn test_backfill_short_ids_skip_existing() {
    // T-1.8: t1, t2 already set, 3 NULL -> only NULL ones get t3, t4, t5
    let conn = setup();
    for i in 1..=2 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("topic-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: format!("2026-01-01T00:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("t{i}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    for i in 3..=5 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("topic-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: format!("2026-01-01T00:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: None,
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let count = backfill_short_ids(&conn).unwrap();
    assert_eq!(count, 3);
    // t1, t2 unchanged
    let node = get_index_node(&conn, "topic-1").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t1".to_string()));
    // New ones got t3, t4, t5
    let node = get_index_node(&conn, "topic-3").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t3".to_string()));
    let node = get_index_node(&conn, "topic-5").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t5".to_string()));
}

#[test]
fn test_backfill_short_ids_empty_table() {
    // T-1.9: No nodes -> 0 changes, no error
    let conn = setup();
    let count = backfill_short_ids(&conn).unwrap();
    assert_eq!(count, 0);
}

// ============================================
// T-1.10 ~ T-1.12: date_from/date_to backfill tests
// TODO: These tests require session_log data infrastructure setup.
//       Implement when session_log-based date inference is added.
// ============================================

// ============================================
// get_index_node_by_short_or_id tests (T-1.13 ~ T-1.15)
// ============================================

#[test]
fn test_get_index_node_by_short_id() {
    // T-1.13: Search by short_id "t42"
    let conn = setup();
    insert_index_node(
        &conn,
        &IndexNodeRow {
            id: "topic-agent:nostarou:main-sess_abc-1-20".to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: String::new(),
            title: "Test Topic".to_string(),
            summary: "test summary".to_string(),
            start_log_id: None,
            end_log_id: None,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: Some("t42".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
    let result = get_index_node_by_short_or_id(&conn, "a1", "t42").unwrap();
    assert!(result.is_some());
    assert_eq!(
        result.unwrap().id,
        "topic-agent:nostarou:main-sess_abc-1-20"
    );
}

#[test]
fn test_get_index_node_by_full_id() {
    // T-1.14: Search by full id
    let conn = setup();
    insert_index_node(
        &conn,
        &IndexNodeRow {
            id: "topic-agent:nostarou:main-sess_abc-1-20".to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: String::new(),
            title: "Test Topic".to_string(),
            summary: "test summary".to_string(),
            start_log_id: None,
            end_log_id: None,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: Some("t42".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
    let result =
        get_index_node_by_short_or_id(&conn, "a1", "topic-agent:nostarou:main-sess_abc-1-20")
            .unwrap();
    assert!(result.is_some());
    assert_eq!(
        result.unwrap().id,
        "topic-agent:nostarou:main-sess_abc-1-20"
    );
}

#[test]
fn test_get_index_node_by_short_id_not_found() {
    // T-1.15: Non-existent short_id returns None
    let conn = setup();
    let result = get_index_node_by_short_or_id(&conn, "a1", "t99999").unwrap();
    assert!(result.is_none());
}

// ============================================
// memory_index_fts / キーワード逆引きテスト
// ============================================

fn mk_topic_node(
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

fn fts_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM memory_index_fts", [], |r| r.get(0))
        .unwrap()
}

fn nodes_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM memory_index_nodes", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn test_index_fts_consistency_through_write_paths() {
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node(
            "t1",
            "a1",
            "Discord連携",
            "botの実装",
            &["Discord", "serenity"],
        ),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &mk_topic_node("t2", "a1", "料理の話", "カレーを作った", &["カレー"]),
    )
    .unwrap();
    assert_eq!(fts_count(&conn), nodes_count(&conn));

    // キーワードでヒット
    let hits = search_index_nodes(&conn, "a1", "serenity", 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "t1");

    // summary 更新が検索に反映される
    update_index_node_summary(&conn, "t2", "料理の話", "肉じゃがを作った").unwrap();
    assert!(
        search_index_nodes(&conn, "a1", "肉じゃが", 10, None)
            .unwrap()
            .len()
            == 1
    );
    assert!(search_index_nodes(&conn, "a1", "カレーを作った", 10, None)
        .unwrap()
        .is_empty());
    assert_eq!(fts_count(&conn), nodes_count(&conn));

    // keywords 更新が検索に反映される
    update_index_node_keywords(&conn, "t2", "[\"肉じゃが\",\"じゃがいも\"]").unwrap();
    let hits = search_index_nodes(&conn, "a1", "じゃがいも", 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "t2");

    // 単体削除で FTS も消える
    delete_index_node(&conn, "t1").unwrap();
    assert!(search_index_nodes(&conn, "a1", "serenity", 10, None)
        .unwrap()
        .is_empty());
    assert_eq!(fts_count(&conn), nodes_count(&conn));

    // agent 単位 purge で FTS も消える
    delete_index_nodes_for_agent(&conn, "a1").unwrap();
    assert_eq!(fts_count(&conn), 0);
    assert_eq!(nodes_count(&conn), 0);
}

#[test]
fn test_index_fts_insert_or_ignore_keeps_existing() {
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "元タイトル", "元要約", &["元"]),
    )
    .unwrap();
    // 同一 id の再 insert は OR IGNORE で無視され、FTS も元のまま
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "新タイトル", "新要約", &["新"]),
    )
    .unwrap();
    assert_eq!(fts_count(&conn), 1);
    assert_eq!(
        search_index_nodes(&conn, "a1", "元要約", 10, None)
            .unwrap()
            .len(),
        1
    );
    assert!(search_index_nodes(&conn, "a1", "新要約", 10, None)
        .unwrap()
        .is_empty());
}

#[test]
fn test_search_index_nodes_scoping_and_filters() {
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "Rust勉強会", "所有権の話", &["Rust"]),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &mk_topic_node("t2", "a2", "Rust輪読", "他人の記憶", &["Rust"]),
    )
    .unwrap();
    let mut period = mk_topic_node("p1", "a1", "2026-05", "5月のRustまとめ", &[]);
    period.node_type = "period".to_string();
    insert_index_node(&conn, &period).unwrap();

    // agent 分離: a1 からは a2 のノードが見えない
    let hits = search_index_nodes(&conn, "a1", "Rust", 10, None).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.node_id != "t2"));

    // node_type フィルタ
    let hits = search_index_nodes(&conn, "a1", "Rust", 10, Some("period")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "p1");

    // AND で 0 件 → OR フォールバックで拾う
    let hits = search_index_nodes(&conn, "a1", "所有権 存在しない語", 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "t1");

    // 空クエリは空結果
    assert!(search_index_nodes(&conn, "a1", "   ", 10, None)
        .unwrap()
        .is_empty());
}

#[test]
fn test_list_topics_missing_keywords() {
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "キーワードなし", "s", &[]),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &mk_topic_node("t2", "a1", "キーワードあり", "s", &["kw"]),
    )
    .unwrap();
    let mut daily = mk_topic_node("d1", "a1", "daily由来", "s", &[]);
    daily.source_type = "daily_log".to_string();
    insert_index_node(&conn, &daily).unwrap();

    let missing = list_topics_missing_keywords(&conn, "a1", 10).unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].id, "t1");
}

#[test]
fn test_search_index_nodes_short_query_like_fallback() {
    // trigram は 3 文字未満の語に当たらない → LIKE フォールバックで拾う
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "AI導入の相談", "LLMの選定", &["AI"]),
    )
    .unwrap();
    let hits = search_index_nodes(&conn, "a1", "AI", 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "t1");
    // LIKE フォールバックでも agent 分離は効く
    assert!(search_index_nodes(&conn, "a2", "AI", 10, None)
        .unwrap()
        .is_empty());
}

#[test]
fn test_delete_index_node_cascades_fts_for_subtree() {
    // parent_id の ON DELETE CASCADE で子孫ノードが消えるとき、FTS も部分木ごと消える
    let conn = setup();
    let mut parent = mk_topic_node("s1", "a1", "親セッション", "親要約", &[]);
    parent.node_type = "session".to_string();
    insert_index_node(&conn, &parent).unwrap();
    let mut child = mk_topic_node("t1", "a1", "子トピック", "子要約ユニーク", &["子kw"]);
    child.parent_id = Some("s1".to_string());
    insert_index_node(&conn, &child).unwrap();
    assert_eq!(nodes_count(&conn), 2);
    assert_eq!(fts_count(&conn), 2);

    delete_index_node(&conn, "s1").unwrap();
    // CASCADE で子も消え、FTS に孤児が残らない
    assert_eq!(nodes_count(&conn), 0);
    assert_eq!(fts_count(&conn), 0);
    assert!(search_index_nodes(&conn, "a1", "子要約ユニーク", 10, None)
        .unwrap()
        .is_empty());
}

#[test]
fn test_index_write_helpers_work_inside_outer_transaction() {
    // index_builder::delete_index はトランザクション内から delete_index_nodes_for_agent
    // を呼ぶ。SAVEPOINT 方式なので外側 tx があっても動くこと（BEGIN の入れ子は不可）。
    let conn = setup();
    insert_index_node(&conn, &mk_topic_node("t1", "a1", "T", "S", &["kw"])).unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    delete_index_nodes_for_agent(&tx, "a1").unwrap();
    insert_index_node(&tx, &mk_topic_node("t2", "a1", "T2", "S2", &["kw2"])).unwrap();
    tx.commit().unwrap();
    assert_eq!(nodes_count(&conn), 1);
    assert_eq!(fts_count(&conn), 1);
}

#[test]
fn test_skill_usage_log_and_last_consolidation() {
    let conn = setup();
    // スキル利用のセッション単位記録
    insert_skill_usage(&conn, "a1", "sk1", "sess-A").unwrap();
    insert_skill_usage(&conn, "a1", "sk1", "sess-B").unwrap();
    insert_skill_usage(&conn, "a1", "sk2", "sess-A").unwrap();
    let mut sk1 = list_skill_used_sessions(&conn, "sk1", None).unwrap();
    sk1.sort();
    assert_eq!(sk1, vec!["sess-A".to_string(), "sess-B".to_string()]);
    assert_eq!(
        list_skill_used_sessions(&conn, "sk2", None).unwrap().len(),
        1
    );
    // since フィルタ（未来時刻なら0件）
    let future = "2999-01-01T00:00:00+00:00";
    assert!(list_skill_used_sessions(&conn, "sk1", Some(future))
        .unwrap()
        .is_empty());

    // last_skill_consolidation_at: 行が無ければ None、UPSERT で行を作って永続化
    assert!(get_last_skill_consolidation_at(&conn, "a1")
        .unwrap()
        .is_none());
    set_last_skill_consolidation_at(&conn, "a1", "2026-07-01T00:00:00+00:00").unwrap();
    assert_eq!(
        get_last_skill_consolidation_at(&conn, "a1")
            .unwrap()
            .as_deref(),
        Some("2026-07-01T00:00:00+00:00")
    );
    // 2回目はフィールドのみ更新
    set_last_skill_consolidation_at(&conn, "a1", "2026-07-02T00:00:00+00:00").unwrap();
    assert_eq!(
        get_last_skill_consolidation_at(&conn, "a1")
            .unwrap()
            .as_deref(),
        Some("2026-07-02T00:00:00+00:00")
    );
}
