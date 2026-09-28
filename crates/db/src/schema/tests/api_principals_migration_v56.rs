#[test]
fn v56_creates_api_principals_on_fresh_and_upgraded_databases() {
    let fresh = crate::init_memory().expect("fresh schema must initialize");
    assert_eq!(
        schema_version(&fresh).expect("fresh user_version"),
        latest_version(),
        "S8 requires the ordinary v56 migration before offline projection"
    );
    assert!(
        table_exists(&fresh, "api_principals").expect("inspect fresh api_principals"),
        "v56 fresh schema must create api_principals"
    );

    let upgraded = crate::init_memory().expect("upgrade fixture");
    upgraded
        .execute_batch("DROP TABLE IF EXISTS api_principals; PRAGMA user_version=55;")
        .expect("prepare v55 fixture");
    initialize(&upgraded).expect("upgrade v55 to latest");
    assert_eq!(schema_version(&upgraded).unwrap(), latest_version());
    assert!(table_exists(&upgraded, "api_principals").unwrap());

    let columns: Vec<(String, String, i64, i64)> = upgraded
        .prepare("PRAGMA table_info(api_principals)")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(1)?, row.get(2)?, row.get(3)?, row.get(5)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        columns,
        vec![
            ("id".into(), "TEXT".into(), 0, 1),
            ("user_id".into(), "TEXT".into(), 1, 0),
            ("agent_id".into(), "TEXT".into(), 1, 0),
            ("permission".into(), "TEXT".into(), 1, 0),
            ("created_by".into(), "TEXT".into(), 1, 0),
            ("created_at".into(), "TEXT".into(), 1, 0),
            ("display_name".into(), "TEXT".into(), 1, 0),
        ],
        "api_principals columns must match the approved S8 contract exactly"
    );
    // `id TEXT PRIMARY KEY` has its own SQLite auto-index. Count only indexes whose
    // origin is an explicit UNIQUE constraint so this assertion targets exactly the
    // approved `(user_id, agent_id)` constraint.
    let composite_unique_count: i64 = upgraded
        .query_row(
            "SELECT COUNT(*) FROM pragma_index_list('api_principals') WHERE origin='u'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        composite_unique_count, 1,
        "(user_id,agent_id) must have exactly one explicit UNIQUE constraint"
    );
}
