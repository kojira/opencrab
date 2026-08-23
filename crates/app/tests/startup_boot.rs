//! Isolated social-runtime boot smoke (#784). Own socket, no gateways.
//!
//! (a) fresh empty DB boots and serves one read; no inplace-v1 marker is written
//! (b) used new-structure DB (events/subjects rows, no marker) boots and serves one read
//! (c) agents table without marker refuses with the migration message
//! (d) agents table plus inplace-v1 marker boots and serves one read

use opencrab_converter::IN_PLACE_MIGRATION_ID;
use opencrab_store::Store;
use rusqlite::Connection;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn scratch(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("oc784-{}-{}-{}", std::process::id(), n, tag));
    assert!(
        path.as_os_str().len() < 100,
        "Unix socket path too long: {}",
        path.display()
    );
    path
}

fn spawn_runtime(sock: &Path, db: &Path) -> Proc {
    let child = Command::new(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .arg(sock)
        .arg(db)
        .arg("room:main")
        .env_remove("OPENCRAB_PLACES")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn opencrab-social-runtime");
    Proc(child)
}

fn inplace_v1_count(db: &Path) -> i64 {
    let conn = Connection::open(db).expect("open db for marker check");
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migration_state'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    if exists == 0 {
        return 0;
    }
    conn.query_row(
        "SELECT COUNT(*) FROM schema_migration_state WHERE migration_id=?1",
        [IN_PLACE_MIGRATION_ID],
        |row| row.get(0),
    )
    .unwrap()
}

fn wait_connect(sock: &Path, timeout: Duration) -> UnixStream {
    let start = Instant::now();
    loop {
        match UnixStream::connect(sock) {
            Ok(stream) => return stream,
            Err(error) => {
                if start.elapsed() >= timeout {
                    panic!("runtime socket not ready: {error}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn wait_id(reader: &mut BufReader<UnixStream>, id: &str) -> Value {
    let start = Instant::now();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => panic!("runtime closed before {id}"),
            Ok(_) => {
                if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                    if value.get("id").and_then(Value::as_str) == Some(id) {
                        return value;
                    }
                }
            }
            Err(error) => {
                if start.elapsed() >= Duration::from_secs(10) {
                    panic!("timeout waiting for {id}: {error}");
                }
            }
        }
        if start.elapsed() >= Duration::from_secs(10) {
            panic!("timeout waiting for {id}");
        }
    }
}

/// Hello as web, then protocol `read` of the provisioned room. No gateway process.
fn serve_one_read(sock: &Path) -> Value {
    let stream = wait_connect(sock, Duration::from_secs(15));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut writer = stream.try_clone().expect("clone unix stream");
    let mut reader = BufReader::new(stream);
    writeln!(
        writer,
        r#"{{"id":"h1","m":"hello","protocol":1,"name":"web","address_form":"room:.+","tools":[],"effects":["say"],"capabilities":[]}}"#
    )
    .unwrap();
    let hello = wait_id(&mut reader, "h1");
    assert!(
        hello.get("ok").is_some(),
        "hello must be ok (no gateway): {hello}"
    );
    writeln!(
        writer,
        r#"{{"id":"r1","m":"read","address":"room:main","from":1}}"#
    )
    .unwrap();
    let read = wait_id(&mut reader, "r1");
    assert!(read.get("ok").is_some(), "read must be ok: {read}");
    read
}

fn seed_used_new_structure(db: &Path) {
    let store = Store::open(db).expect("create store schema");
    drop(store);
    let conn = Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO subjects(kind,name,persona,turn_runner,standing,created_at)
         VALUES('agent','used','p','echo','trusted',1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO events(place_id,seq,kind,content_json,mentions_json,created_at)
         VALUES(1,1,'said','{}','[]',1)",
        [],
    )
    .unwrap();
}

fn seed_agents(db: &Path) {
    let conn = Connection::open(db).unwrap();
    conn.execute("CREATE TABLE agents(agent_id TEXT NOT NULL)", [])
        .unwrap();
}

fn seed_inplace_v1(db: &Path) {
    let conn = Connection::open(db).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migration_state(
           migration_id TEXT NOT NULL PRIMARY KEY,
           applied_at INTEGER NOT NULL,
           source_row_digest BLOB
         );
         INSERT INTO schema_migration_state(migration_id, applied_at)
         VALUES('inplace-v1', 1);",
    )
    .unwrap();
}

fn boot_and_read(db: &Path) -> Value {
    let sock = scratch("s.sock");
    let _ = std::fs::remove_file(&sock);
    let runtime = spawn_runtime(&sock, db);
    let read = serve_one_read(&sock);
    drop(runtime);
    let _ = std::fs::remove_file(&sock);
    read
}

#[test]
fn fresh_empty_db_boots_serves_read_without_marker() {
    let db = scratch("a.db");
    let _ = std::fs::remove_file(&db);
    let read = boot_and_read(&db);
    assert!(
        read["ok"]["events"].as_array().is_some(),
        "fresh boot must serve a read page: {read}"
    );
    assert_eq!(
        inplace_v1_count(&db),
        0,
        "fresh boot must not write a marker"
    );
}

#[test]
fn used_new_structure_db_boots_serves_read_without_marker() {
    let db = scratch("b.db");
    let _ = std::fs::remove_file(&db);
    seed_used_new_structure(&db);
    assert_eq!(inplace_v1_count(&db), 0);
    let read = boot_and_read(&db);
    assert!(
        read["ok"]["events"].as_array().is_some(),
        "used new-structure boot must serve a read page: {read}"
    );
    assert_eq!(
        inplace_v1_count(&db),
        0,
        "used new-structure boot must not write a marker"
    );
}

#[test]
fn agents_table_without_marker_refuses_to_serve() {
    let db = scratch("c.db");
    let sock = scratch("c.sock");
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&sock);
    seed_agents(&db);
    let output = Command::new(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .arg(&sock)
        .arg(&db)
        .arg("room:main")
        .env_remove("OPENCRAB_PLACES")
        .output()
        .expect("run opencrab-social-runtime");
    assert!(
        !output.status.success(),
        "legacy-implementation DB must not serve"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("this is a legacy-implementation DB; run the migration"),
        "refuse message missing: {stderr}"
    );
}

#[test]
fn agents_table_with_marker_boots_and_serves_read() {
    let db = scratch("d.db");
    let _ = std::fs::remove_file(&db);
    seed_agents(&db);
    seed_inplace_v1(&db);
    let read = boot_and_read(&db);
    assert!(
        read["ok"]["events"].as_array().is_some(),
        "migrated body DB must serve a read page: {read}"
    );
    assert_eq!(inplace_v1_count(&db), 1);
}
