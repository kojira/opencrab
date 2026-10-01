use super::prompt::build_agent_context;
use opencrab_actions::CallerIdentity;
use opencrab_db::queries::SkillRow;

fn insert_skill(conn: &rusqlite::Connection, name: &str, agent_visible: bool) {
    let row = SkillRow {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: "a1".to_string(),
        name: name.to_string(),
        description: format!("{name} desc"),
        situation_pattern: "sp".to_string(),
        guidance: "g".to_string(),
        source_type: "experience".to_string(),
        source_context: None,
        file_path: None,
        effectiveness: None,
        usage_count: 0,
        is_active: true,
        permission: "\"agent\"".to_string(),
        archived: false,
        created_caller: None,
        agent_visible,
    };
    opencrab_db::queries::insert_skill(conn, &row).unwrap();
}

#[test]
fn agent_caller_sees_only_visible_skills_in_index() {
    let conn = opencrab_db::init_memory().unwrap();
    insert_skill(&conn, "VisibleSkill", true);
    insert_skill(&conn, "HiddenSkill", false);

    let (agent_prompt, _) = build_agent_context(&conn, "a1", &CallerIdentity::Agent);
    assert!(
        agent_prompt.contains("VisibleSkill"),
        "visible skill missing from agent index:\n{agent_prompt}"
    );
    assert!(
        !agent_prompt.contains("HiddenSkill"),
        "hidden skill leaked into agent index:\n{agent_prompt}"
    );

    // Owner / CoAgent / TrustedUser は両方見える（従来どおり / 絞りは caller=Agent のみ）。
    for caller in [
        CallerIdentity::Owner,
        CallerIdentity::CoAgent {
            agent_id: "peer".to_string(),
        },
        CallerIdentity::TrustedUser,
    ] {
        let (p, _) = build_agent_context(&conn, "a1", &caller);
        assert!(
            p.contains("VisibleSkill") && p.contains("HiddenSkill"),
            "caller {caller:?} must see all skills:\n{p}"
        );
    }
}

#[test]
fn agent_caller_with_no_visible_skills_gets_no_skill_section() {
    let conn = opencrab_db::init_memory().unwrap();
    // 既定 false のみ = Agent には 1 件も見えない。
    insert_skill(&conn, "HiddenOnly", false);

    let (agent_prompt, _) = build_agent_context(&conn, "a1", &CallerIdentity::Agent);
    assert!(!agent_prompt.contains("HiddenOnly"));
    // 空の見出しだけ残さない（セクションごと出さない）。
    assert!(
        !agent_prompt.contains("Your skills (index only"),
        "empty skill section header must not appear for agent caller:\n{agent_prompt}"
    );

    // 同じ DB でも Owner にはセクションと skill が出る。
    let (owner_prompt, _) = build_agent_context(&conn, "a1", &CallerIdentity::Owner);
    assert!(owner_prompt.contains("Your skills (index only"));
    assert!(owner_prompt.contains("HiddenOnly"));
}

/// D-1056: skill index は caller で絞られるので、curated memory まで含む固定部の後ろ
/// （system の 2 番目のセグメント）に置く。並びは使用回数ではなく名前順で、使うたびに
/// 並びが変わってプロンプトキャッシュが外れないこと。
#[test]
fn skill_index_is_name_sorted_in_caller_segment_after_curated() {
    let conn = opencrab_db::init_memory().unwrap();
    insert_skill(&conn, "Zeta", true);
    insert_skill(&conn, "Alpha", true);
    insert_skill(&conn, "Mid", true);
    // 使用回数順なら Mid が先頭に来る。
    conn.execute("UPDATE skills SET usage_count = 99 WHERE name = 'Mid'", [])
        .unwrap();
    opencrab_db::queries::upsert_curated_memory(
        &conn,
        &opencrab_db::queries::CuratedMemoryRow {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: "a1".to_string(),
            category: "long_term/Topic".to_string(),
            content: "- curated fact".to_string(),
            created_at: String::new(),
        },
    )
    .unwrap();

    let (prompt, _) = build_agent_context(&conn, "a1", &CallerIdentity::Owner);
    let segments: Vec<&str> = prompt
        .split(opencrab_llm_types::SYSTEM_SEGMENT_BREAK)
        .collect();
    assert_eq!(
        segments.len(),
        2,
        "stable と caller の 2 セグメント:\n{prompt}"
    );
    assert!(segments[0].contains("- curated fact"));
    assert!(!segments[0].contains("Your skills (index only"));
    let skills = segments[1];
    assert!(skills.starts_with("Your skills (index only"));
    let pos = |n: &str| skills.find(&format!("- {n}:")).unwrap();
    assert!(
        pos("Alpha") < pos("Mid") && pos("Mid") < pos("Zeta"),
        "{skills}"
    );
}
