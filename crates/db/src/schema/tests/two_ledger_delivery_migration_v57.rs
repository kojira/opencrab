#[test]
fn s7_v57_adds_immutable_generic_delivery_evidence() {
    let conn = Connection::open_in_memory().unwrap();
    initialize(&conn).unwrap();
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, 57);
    let cols: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(deliveries)").unwrap();
        stmt.query_map([], |r| r.get(1)).unwrap().map(Result::unwrap).collect()
    };
    for required in [
        "payload_digest",
        "delivery_guarantee",
        "prepared_protocol_digest",
        "acknowledged_at",
        "frame_kind",
        "prepared_frame_json",
    ] {
        assert!(cols.iter().any(|col| col == required), "missing {required}");
    }
}
