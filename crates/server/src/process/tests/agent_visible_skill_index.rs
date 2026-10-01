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

fn all_callers() -> [CallerIdentity; 4] {
    [
        CallerIdentity::Owner,
        CallerIdentity::Agent,
        CallerIdentity::CoAgent {
            agent_id: "peer".to_string(),
        },
        CallerIdentity::TrustedUser,
    ]
}

/// D-1058: skill index は caller に依らず同じ。露出許可（agent_visible）の無い skill の名前は
/// 誰のターンでも出さず、`owner-skills` の 1 行に畳む（#352 の名前を隠す要件を保つ）。
#[test]
fn skill_index_is_identical_for_all_callers_and_hides_owner_only_names() {
    let conn = opencrab_db::init_memory().unwrap();
    insert_skill(&conn, "VisibleSkill", true);
    insert_skill(&conn, "HiddenSkill", false);

    let (owner_prompt, _) = build_agent_context(&conn, "a1", &CallerIdentity::Owner);
    assert!(
        owner_prompt.contains("- VisibleSkill: VisibleSkill desc"),
        "{owner_prompt}"
    );
    assert!(
        !owner_prompt.contains("HiddenSkill"),
        "owner-only skill name must not be listed:\n{owner_prompt}"
    );
    assert!(
        owner_prompt.contains("- owner-skills: Index of owner-only skills"),
        "{owner_prompt}"
    );
    for caller in all_callers() {
        let (p, _) = build_agent_context(&conn, "a1", &caller);
        assert_eq!(
            p, owner_prompt,
            "caller {caller:?} must get the same prompt"
        );
    }
}

/// D-1058: 露出許可の無い skill だけなら一覧は `owner-skills` 1 行。1 件も無ければ
/// `owner-skills` 行は出さず、skill が 1 件も無ければ見出しごと出さない。
#[test]
fn skill_index_owner_skills_line_only_when_owner_only_skills_exist() {
    let conn = opencrab_db::init_memory().unwrap();
    let (empty, _) = build_agent_context(&conn, "a1", &CallerIdentity::Agent);
    assert!(!empty.contains("Your skills (index only"), "{empty}");

    insert_skill(&conn, "VisibleOnly", true);
    let (visible_only, _) = build_agent_context(&conn, "a1", &CallerIdentity::Agent);
    assert!(visible_only.contains("- VisibleOnly:"));
    assert!(!visible_only.contains("owner-skills"), "{visible_only}");

    let conn = opencrab_db::init_memory().unwrap();
    insert_skill(&conn, "HiddenOnly", false);
    let (hidden_only, _) = build_agent_context(&conn, "a1", &CallerIdentity::Agent);
    assert!(hidden_only.contains("Your skills (index only"));
    assert!(!hidden_only.contains("HiddenOnly"), "{hidden_only}");
    assert!(hidden_only.contains("- owner-skills:"), "{hidden_only}");
}

/// D-1058: skill index は caller に依らないので固定部（curated の後ろ）に置く。caller 依存部は
/// 空のまま区切りだけ残る。並びは使用回数ではなく名前順で、`owner-skills` は末尾に固定。
#[test]
fn skill_index_is_name_sorted_in_stable_segment_after_curated() {
    let conn = opencrab_db::init_memory().unwrap();
    insert_skill(&conn, "Zeta", true);
    insert_skill(&conn, "Alpha", true);
    insert_skill(&conn, "Mid", true);
    insert_skill(&conn, "Hidden", false);
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
        "stable と（空の）caller の 2 セグメント:\n{prompt}"
    );
    assert!(segments[1].is_empty(), "caller segment must be empty");
    let stable = segments[0];
    let skills = &stable[stable.find("Your skills (index only").unwrap()..];
    assert!(stable.find("- curated fact").unwrap() < stable.find("Your skills").unwrap());
    let pos = |n: &str| skills.find(&format!("- {n}:")).unwrap();
    assert!(
        pos("Alpha") < pos("Mid") && pos("Mid") < pos("Zeta") && pos("Zeta") < pos("owner-skills"),
        "{skills}"
    );
}
