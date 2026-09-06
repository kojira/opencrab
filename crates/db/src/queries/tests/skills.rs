use super::*;

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
        created_caller: None,
        agent_visible: false,
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

// 9b. created_caller が insert / select / update で往復すること（#335）。
#[test]
fn test_skill_created_caller_roundtrip() {
    let conn = setup();

    let mut skill = SkillRow {
        id: "skill-cc".to_string(),
        agent_id: "agent-1".to_string(),
        name: "Gated".to_string(),
        description: "d".to_string(),
        situation_pattern: String::new(),
        guidance: "g".to_string(),
        source_type: "self_created".to_string(),
        source_context: None,
        file_path: None,
        effectiveness: None,
        usage_count: 0,
        is_active: true,
        permission: "\"agent\"".to_string(),
        archived: false,
        created_caller: Some("agent".to_string()),
        agent_visible: false,
    };
    insert_skill(&conn, &skill).unwrap();

    let got = find_skill_by_id(&conn, "skill-cc").unwrap().unwrap();
    assert_eq!(got.created_caller.as_deref(), Some("agent"));

    // update_skill も created_caller を書き戻す。
    skill.created_caller = Some("owner".to_string());
    update_skill(&conn, &skill).unwrap();
    let got = find_skill_by_id(&conn, "skill-cc").unwrap().unwrap();
    assert_eq!(got.created_caller.as_deref(), Some("owner"));

    // NULL（legacy）も往復する。
    let legacy = SkillRow {
        id: "skill-legacy".to_string(),
        created_caller: None,
        ..skill.clone()
    };
    // 別 id / 別 name で入れ直す（同名 UNIQUE 衝突回避）。
    let legacy = SkillRow {
        name: "LegacyGate".to_string(),
        ..legacy
    };
    insert_skill(&conn, &legacy).unwrap();
    let got = find_skill_by_id(&conn, "skill-legacy").unwrap().unwrap();
    assert!(got.created_caller.is_none());
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
        created_caller: None,
        agent_visible: false,
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
        created_caller: None,
        agent_visible: false,
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
    assert!(found.unwrap().archived);
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
        created_caller: None,
        agent_visible: false,
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
    assert!(!s.archived);
    assert!(s.is_active);
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
