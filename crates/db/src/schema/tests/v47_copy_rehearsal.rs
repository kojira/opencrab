use rusqlite::types::ValueRef;
use rusqlite::OpenFlags;
use std::collections::BTreeSet;

const V47_NEW_TABLES: &[&str] = &[
    "conversation_snapshots",
    "deliveries",
    "external_origins",
    "gate_bindings",
    "gate_instances",
    "gateway_operation_calls",
    "nostr_bundle_state",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct RehearsalColumn {
    cid: i64,
    name: String,
    kind: String,
    not_null: i64,
    default_value: Option<String>,
    primary_key: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RehearsalTable {
    count: i64,
    digest: String,
    columns: Vec<RehearsalColumn>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RehearsalIndex {
    name: String,
    unique: i64,
    origin: String,
    partial: i64,
    columns: Vec<(i64, i64, Option<String>)>,
    sql: String,
}

type RehearsalForeignKey = (i64, i64, String, String, String, String, String, String);
type RehearsalObject = (String, String, String, String);

#[derive(Debug, Clone, PartialEq, Eq)]
struct RehearsalTableCatalog {
    columns: Vec<RehearsalColumn>,
    foreign_keys: Vec<RehearsalForeignKey>,
    indexes: Vec<RehearsalIndex>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RehearsalCatalog {
    tables: BTreeMap<String, RehearsalTableCatalog>,
    objects: Vec<RehearsalObject>,
    subject_id: Vec<RehearsalColumn>,
}

fn rehearsal_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<RehearsalColumn>> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let columns = statement
        .query_map([], |row| {
            Ok(RehearsalColumn {
                cid: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                not_null: row.get(3)?,
                default_value: row.get(4)?,
                primary_key: row.get(5)?,
            })
        })?
        .collect();
    columns
}

fn rehearsal_frame(hasher: &mut Sha256, tag: u8, payload: &[u8]) {
    hasher.update([tag]);
    hasher.update((payload.len() as u64).to_be_bytes());
    hasher.update(payload);
}

fn rehearsal_table_digest(
    conn: &Connection,
    table: &str,
    selected: &[RehearsalColumn],
) -> rusqlite::Result<RehearsalTable> {
    let mut primary_key: Vec<_> = selected
        .iter()
        .filter(|column| column.primary_key > 0)
        .map(|column| (column.primary_key, column.name.as_str()))
        .collect();
    primary_key.sort_unstable();
    let order = if primary_key.is_empty() {
        "rowid".to_string()
    } else {
        primary_key
            .iter()
            .map(|(_, name)| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let names = selected
        .iter()
        .map(|column| format!("\"{}\"", column.name))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {names} FROM \"{table}\" ORDER BY {order}");
    let count = conn.query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| {
        row.get(0)
    })?;
    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.query([])?;
    let mut hasher = Sha256::new();
    while let Some(row) = rows.next()? {
        hasher.update(b"R");
        for (index, column) in selected.iter().enumerate() {
            rehearsal_frame(&mut hasher, b'N', column.name.as_bytes());
            match row.get_ref(index)? {
                ValueRef::Null => rehearsal_frame(&mut hasher, b'0', &[]),
                ValueRef::Integer(value) => {
                    rehearsal_frame(&mut hasher, b'I', &value.to_be_bytes())
                }
                ValueRef::Real(value) => {
                    rehearsal_frame(&mut hasher, b'F', &value.to_bits().to_be_bytes())
                }
                ValueRef::Text(value) => rehearsal_frame(&mut hasher, b'T', value),
                ValueRef::Blob(value) => rehearsal_frame(&mut hasher, b'B', value),
            }
        }
    }
    Ok(RehearsalTable {
        count,
        digest: format!("{:x}", hasher.finalize()),
        columns: selected.to_vec(),
    })
}

fn rehearsal_snapshot(conn: &Connection) -> rusqlite::Result<BTreeMap<String, RehearsalTable>> {
    let mut snapshot = BTreeMap::new();
    for table in user_tables(conn) {
        let columns = rehearsal_columns(conn, &table)?;
        snapshot.insert(
            table.clone(),
            rehearsal_table_digest(conn, &table, &columns)?,
        );
    }
    Ok(snapshot)
}

fn rehearsal_normalized_sql(sql: Option<String>) -> String {
    sql.unwrap_or_default()
        .replace("IF NOT EXISTS", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn rehearsal_indexes(conn: &Connection, table: &str) -> rusqlite::Result<Vec<RehearsalIndex>> {
    let mut statement = conn.prepare(&format!("PRAGMA index_list(\"{table}\")"))?;
    let rows: Vec<(String, i64, String, i64)> = statement
        .query_map([], |row| {
            Ok((row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut indexes = Vec::new();
    for (name, unique, origin, partial) in rows {
        let mut info = conn.prepare(&format!("PRAGMA index_info(\"{name}\")"))?;
        let columns = info
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let sql = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='index' AND name=?1",
                [&name],
                |row| row.get(0),
            )
            .unwrap_or(None);
        indexes.push(RehearsalIndex {
            name,
            unique,
            origin,
            partial,
            columns,
            sql: rehearsal_normalized_sql(sql),
        });
    }
    indexes.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(indexes)
}

fn rehearsal_catalog(conn: &Connection) -> rusqlite::Result<RehearsalCatalog> {
    let mut tables = BTreeMap::new();
    for table in V47_NEW_TABLES {
        let mut foreign_keys = conn
            .prepare(&format!("PRAGMA foreign_key_list(\"{table}\")"))?
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        foreign_keys.sort();
        tables.insert(
            (*table).to_string(),
            RehearsalTableCatalog {
                columns: rehearsal_columns(conn, table)?,
                foreign_keys,
                indexes: rehearsal_indexes(conn, table)?,
            },
        );
    }
    let placeholders = std::iter::repeat_n("?", V47_NEW_TABLES.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT type, name, tbl_name, sql FROM sqlite_master
         WHERE type IN ('index', 'trigger') AND tbl_name IN ({placeholders})
         ORDER BY type, name"
    );
    let mut statement = conn.prepare(&sql)?;
    let objects = statement
        .query_map(rusqlite::params_from_iter(V47_NEW_TABLES.iter()), |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                rehearsal_normalized_sql(row.get(3)?),
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let subject_id = rehearsal_columns(conn, "agents")?
        .into_iter()
        .filter(|column| column.name == "subject_id")
        .collect();
    Ok(RehearsalCatalog {
        tables,
        objects,
        subject_id,
    })
}

fn rehearsal_integrity(conn: &Connection) -> rusqlite::Result<()> {
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    assert_eq!(integrity, "ok", "integrity_check failed");
    let foreign_key_errors: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_foreign_key_check",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(foreign_key_errors, 0, "foreign_key_check failed");
    Ok(())
}

fn rehearsal_subject_id_violations(conn: &Connection) -> rusqlite::Result<(i64, i64)> {
    let invalid: i64 = conn.query_row(
        "SELECT COUNT(*) FROM agents
         WHERE subject_id IS NULL OR typeof(subject_id) != 'integer' OR subject_id <= 0",
        [],
        |row| row.get(0),
    )?;
    let duplicates: i64 = conn.query_row(
        "SELECT COUNT(*) FROM (
         SELECT subject_id FROM agents GROUP BY subject_id HAVING COUNT(*) > 1)",
        [],
        |row| row.get(0),
    )?;
    Ok((invalid, duplicates))
}

fn rehearsal_verify_subject_ids(conn: &Connection) -> rusqlite::Result<()> {
    let (invalid, duplicates) = rehearsal_subject_id_violations(conn)?;
    assert_eq!(invalid, 0, "invalid agents.subject_id rows");
    assert_eq!(duplicates, 0, "duplicate agents.subject_id groups");
    Ok(())
}

fn rehearsal_open_read_only(path: &std::path::Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
}

fn rehearsal_future_tables(conn: &Connection) -> BTreeSet<&'static str> {
    let tables: BTreeSet<_> = user_tables(conn).into_iter().collect();
    V47_NEW_TABLES
        .iter()
        .filter(|table| tables.contains(**table))
        .copied()
        .collect()
}

#[test]
fn rehearse_v43_copy_to_v47() {
    let directory = match std::env::var("OPENCRAB_V47_REHEARSAL_DIR") {
        Ok(path) if !path.is_empty() => std::path::PathBuf::from(path),
        _ => return,
    };
    let pristine = rehearsal_open_read_only(&directory.join("pristine.db")).expect("pristine");
    assert_eq!(schema_version(&pristine).unwrap(), 43, "source copy must be v43");
    rehearsal_integrity(&pristine).unwrap();
    let before_tables: BTreeSet<_> = user_tables(&pristine).into_iter().collect();
    let future = rehearsal_future_tables(&pristine);
    assert!(future.is_empty(), "v43 source contains future tables: {future:?}");
    let before = rehearsal_snapshot(&pristine).unwrap();
    drop(pristine);

    let a_path = directory.join("a.db");
    let b_path = directory.join("b.db");
    let fresh_path = directory.join("fresh.db");
    let a = crate::init_connection(a_path.to_str().unwrap()).expect("initialize A");
    drop(a);
    let b = crate::init_connection(b_path.to_str().unwrap()).expect("initialize B first");
    drop(b);
    let b = crate::init_connection(b_path.to_str().unwrap()).expect("initialize B second");
    drop(b);
    let fresh = crate::init_connection(fresh_path.to_str().unwrap()).expect("initialize fresh");
    let expected_catalog = rehearsal_catalog(&fresh).unwrap();
    drop(fresh);

    let mut final_snapshots = Vec::new();
    for (label, path) in [("A", &a_path), ("B", &b_path)] {
        let conn = Connection::open(path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 47, "copy {label}");
        rehearsal_integrity(&conn).unwrap();
        let after_tables: BTreeSet<_> = user_tables(&conn).into_iter().collect();
        let expected_tables: BTreeSet<_> = before_tables
            .iter()
            .cloned()
            .chain(V47_NEW_TABLES.iter().map(|name| (*name).to_string()))
            .collect();
        assert_eq!(after_tables, expected_tables, "copy {label} table set");
        for (table, expected) in &before {
            let actual_columns = rehearsal_columns(&conn, table).unwrap();
            let allowed_added: BTreeSet<&str> = if table == "agents" {
                BTreeSet::from(["subject_id"])
            } else {
                BTreeSet::new()
            };
            let before_names: BTreeSet<_> =
                expected.columns.iter().map(|column| column.name.as_str()).collect();
            let added: BTreeSet<_> = actual_columns
                .iter()
                .map(|column| column.name.as_str())
                .filter(|name| !before_names.contains(name))
                .collect();
            assert_eq!(added, allowed_added, "copy {label} columns for {table}");
            let actual = rehearsal_table_digest(&conn, table, &expected.columns).unwrap();
            assert_eq!(&actual, expected, "copy {label} changed existing table {table}");
        }
        for table in V47_NEW_TABLES {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "copy {label} new table {table} is not empty");
        }
        rehearsal_verify_subject_ids(&conn).unwrap();
        assert_eq!(rehearsal_catalog(&conn).unwrap(), expected_catalog, "copy {label} catalog");
        final_snapshots.push(rehearsal_snapshot(&conn).unwrap());
    }
    assert_eq!(final_snapshots[0], final_snapshots[1], "A/B fixed point differs");
}

#[test]
fn v47_rehearsal_rejects_future_table_in_v43_source() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE gateway_operation_calls (wrong TEXT); PRAGMA user_version=43;",
    )
    .unwrap();
    assert_eq!(
        rehearsal_future_tables(&conn),
        BTreeSet::from(["gateway_operation_calls"])
    );
}

#[test]
fn v47_rehearsal_digest_frames_control_bytes_unambiguously() {
    let first = Connection::open_in_memory().unwrap();
    let second = Connection::open_in_memory().unwrap();
    for conn in [&first, &second] {
        conn.execute_batch("CREATE TABLE t (a TEXT, b TEXT);").unwrap();
    }
    first.execute("INSERT INTO t VALUES (?1, ?2)", ["x\u{1}N", "y"]).unwrap();
    second.execute("INSERT INTO t VALUES (?1, ?2)", ["x", "N\u{1}y"]).unwrap();
    let columns = rehearsal_columns(&first, "t").unwrap();
    assert_ne!(
        rehearsal_table_digest(&first, "t", &columns).unwrap(),
        rehearsal_table_digest(&second, "t", &columns).unwrap()
    );
}

#[test]
fn v47_rehearsal_subject_id_rejects_non_integer_storage() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE agents (subject_id); INSERT INTO agents VALUES (1.5);")
        .unwrap();
    assert_eq!(rehearsal_subject_id_violations(&conn).unwrap(), (1, 0));
}
