use super::peer_reviewers_section;
use opencrab_db::queries::TrustedUserPermission;

#[test]
fn roster_lists_co_agents_only_and_handles_empty() {
    let conn = opencrab_db::init_memory().unwrap();
    assert_eq!(peer_reviewers_section(&conn, "a1"), "");

    opencrab_db::queries::add_trusted_user(
        &conn,
        opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
        "r1",
        "a1",
        "42",
        TrustedUserPermission::CoAgent,
        "owner",
        "2026-01-01",
        "Crab B",
    )
    .unwrap();
    opencrab_db::queries::add_trusted_user(
        &conn,
        opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
        "r2",
        "a1",
        "43",
        TrustedUserPermission::CoAgent,
        "owner",
        "2026-01-01",
        "",
    )
    .unwrap();
    opencrab_db::queries::add_trusted_user(
        &conn,
        opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
        "r3",
        "a1",
        "44",
        TrustedUserPermission::User,
        "owner",
        "2026-01-01",
        "Human",
    )
    .unwrap();

    let section = peer_reviewers_section(&conn, "a1");
    // 表示名のみ。メンション記法（transport 固有）は共有プロンプトに出さない（#158 S2）。
    assert!(section.contains("- Crab B"));
    assert!(!section.contains("<@"));
    assert!(!section.contains("42"));
    // 表示名が空の行（id=43）は指名できないので載せない
    assert!(!section.contains("43"));
    assert!(!section.contains("Human"));
    // 他エージェントのロスターには出ない
    assert_eq!(peer_reviewers_section(&conn, "a2"), "");
}

/// 表示名のある co_agent が居なければロスターは空（id だけの行は載らない）。
#[test]
fn roster_is_empty_when_all_display_names_are_blank() {
    let conn = opencrab_db::init_memory().unwrap();
    opencrab_db::queries::add_trusted_user(
        &conn,
        opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
        "r1",
        "a1",
        "42",
        TrustedUserPermission::CoAgent,
        "owner",
        "2026-01-01",
        "",
    )
    .unwrap();
    assert_eq!(peer_reviewers_section(&conn, "a1"), "");
}
