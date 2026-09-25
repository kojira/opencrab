use std::path::Path;

#[test]
fn s8_import_and_project_are_offline_idempotent_and_preserve_core_rows() {
    projected_discord_fixture(false, false);
}

#[test]
fn s10_verify_freeze_is_read_only_for_projected_discord_fixture() {
    projected_discord_fixture(true, false);
}

#[test]
fn s10_verify_freeze_accepts_post_qc_heartbeat_last_fired_advance() {
    projected_discord_fixture(true, true);
}

fn advance_projected_heartbeat(path: &Path) {
    let conn = Connection::open(path).unwrap();
    opencrab_db::queries::set_session_last_fired(
        &conn,
        "agent-a",
        "session-1",
        "2026-01-01T00:10:00Z",
    )
    .unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT last_fired_at FROM session_heartbeat_config WHERE agent_id='agent-a' AND session_id='session-1'",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap()
        .as_deref(),
        Some("2026-01-01T00:10:00Z"),
        "the normal fire path must advance the projected target before post-QC freeze"
    );
}
