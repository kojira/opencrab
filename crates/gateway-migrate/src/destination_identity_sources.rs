const LEGACY_IDENTITY_SOURCES_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS legacy_identity_sources (
  instance_id TEXT NOT NULL,
  id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  permission TEXT NOT NULL,
  created_by TEXT NOT NULL,
  created_at TEXT NOT NULL,
  display_name TEXT NOT NULL,
  platform TEXT NOT NULL,
  PRIMARY KEY(instance_id, id)
);";

const LEGACY_IDENTITY_SOURCE_COLUMNS: &[&str] = &[
    "instance_id", "id", "user_id", "agent_id", "permission", "created_by", "created_at",
    "display_name", "platform",
];

fn legacy_identity_sources_present(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='legacy_identity_sources')",
        [],
        |row| row.get(0),
    )?)
}

fn validate_legacy_identity_sources_if_present(conn: &Connection) -> Result<()> {
    if legacy_identity_sources_present(conn)? {
        source::require_columns(conn, "legacy_identity_sources", LEGACY_IDENTITY_SOURCE_COLUMNS)?;
    }
    Ok(())
}

fn create_legacy_identity_sources(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(LEGACY_IDENTITY_SOURCES_SCHEMA)?;
    source::require_columns(tx, "legacy_identity_sources", LEGACY_IDENTITY_SOURCE_COLUMNS)
}

fn legacy_identity_source_semantic(source: &LegacyIdentitySource) -> Value {
    json!({
        "instance_id": source.instance_id,
        "id": source.id,
        "user_id": source.user_id,
        "agent_id": source.agent_id,
        "permission": source.permission,
        "created_by": source.created_by,
        "created_at": source.created_at,
        "display_name": source.display_name,
        "platform": source.platform,
    })
}

fn current_legacy_identity_source(conn: &Connection, key: &[String]) -> Result<Option<Value>> {
    if !legacy_identity_sources_present(conn)? {
        return Ok(None);
    }
    conn.query_row(
        "SELECT user_id,agent_id,permission,created_by,created_at,display_name,platform \
         FROM legacy_identity_sources WHERE instance_id=?1 AND id=?2",
        params![key[0], key[1]],
        |row| {
            Ok(json!({
                "instance_id": key[0],
                "id": key[1],
                "user_id": row.get::<_, String>(0)?,
                "agent_id": row.get::<_, String>(1)?,
                "permission": row.get::<_, String>(2)?,
                "created_by": row.get::<_, String>(3)?,
                "created_at": row.get::<_, String>(4)?,
                "display_name": row.get::<_, String>(5)?,
                "platform": row.get::<_, String>(6)?,
            }))
        },
    ).optional().map_err(Into::into)
}

fn apply_legacy_identity_source(tx: &Transaction<'_>, source: &LegacyIdentitySource) -> Result<(bool, String)> {
    let key = [source.instance_id.clone(), source.id.clone()];
    let expected = legacy_identity_source_semantic(source);
    let hash = row_hash("legacy_identity_sources", &key, &expected)?;
    if let Some(current) = current_legacy_identity_source(tx, &key)? {
        ensure!(current == expected, "legacy identity source conflict");
        return Ok((false, hash));
    }
    tx.execute(
        "INSERT INTO legacy_identity_sources \
         (instance_id,id,user_id,agent_id,permission,created_by,created_at,display_name,platform) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![source.instance_id, source.id, source.user_id, source.agent_id,
            source.permission, source.created_by, source.created_at, source.display_name, source.platform],
    )?;
    Ok((true, hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_nostr_store_without_inert_table_is_validated_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nostr.db");
        drop(opencrab_nostr_gateway::store::NostrStore::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("DROP TABLE legacy_identity_sources;").unwrap();
        drop(conn);
        let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        require_destination_shape(&conn, &Destination {kind_id: "nostr".into(), path_id: "main".into(), schema: "s5-nostr-v1".into()}).unwrap();
        assert!(!legacy_identity_sources_present(&conn).unwrap());
        drop(conn);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE legacy_identity_sources (instance_id TEXT, id TEXT);").unwrap();
        drop(conn);
        let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert!(validate_legacy_identity_sources_if_present(&conn).is_err());
    }

    #[test]
    fn legacy_identity_sources_use_instance_and_original_id_without_rewriting() {
        let mut conn = Connection::open_in_memory().unwrap();
        let tx = conn.transaction().unwrap();
        create_legacy_identity_sources(&tx).unwrap();
        let first = LegacyIdentitySource {
            instance_id: "first".into(), id: "original-id".into(), user_id: "external".into(),
            agent_id: "agent".into(), permission: "co-agent".into(), created_by: "owner".into(),
            created_at: "2026".into(), display_name: "Source".into(), platform: "external".into(),
        };
        assert!(apply_legacy_identity_source(&tx, &first).unwrap().0);
        assert!(!apply_legacy_identity_source(&tx, &first).unwrap().0);
        let second = LegacyIdentitySource { instance_id: "second".into(), ..first.clone() };
        assert!(apply_legacy_identity_source(&tx, &second).unwrap().0);
        assert_eq!(tx.query_row("SELECT COUNT(*) FROM legacy_identity_sources", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
        let changed = LegacyIdentitySource { display_name: "changed".into(), ..first };
        assert!(apply_legacy_identity_source(&tx, &changed).is_err());
    }
}
