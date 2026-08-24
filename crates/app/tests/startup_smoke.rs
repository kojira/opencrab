//! Binary spawn smoke for lifecycle states (TEST-DESIGN F1 / F2 / F3).

use opencrab_converter::{migrate_in_place, IN_PLACE_MIGRATION_ID};
use opencrab_store::Store;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(10);

fn bin_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .parent()
        .expect("core binary parent")
        .to_path_buf()
}

fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    bin_dir().join(format!("smoke-{}-{}-{}", std::process::id(), n, name))
}

fn write_migration_inputs(dir: &Path) -> (PathBuf, PathBuf) {
    let config = dir.join("empty.toml");
    let environment = dir.join("empty.env");
    std::fs::write(&config, "").unwrap();
    std::fs::write(&environment, "").unwrap();
    (config, environment)
}

struct Spawned {
    child: std::process::Child,
    stderr: String,
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_runtime(socket: &Path, db: &Path) -> Spawned {
    let child = Command::new(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .arg(socket)
        .arg(db)
        .arg("room:main")
        .env_remove("OPENCRAB_PLACES")
        .env_remove("OPENCRAB_LLM_PROVIDER")
        .env_remove("OPENCRAB_MOCK_LLM_SCRIPT")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn opencrab-social-runtime");
    Spawned {
        child,
        stderr: String::new(),
    }
}

fn wait_listen_or_exit(spawned: &mut Spawned, timeout: Duration) -> bool {
    let start = Instant::now();
    let stderr = spawned.child.stderr.take().expect("runtime stderr");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
        {
            let _ = tx.send(line);
        }
    });
    loop {
        while let Ok(line) = rx.try_recv() {
            spawned.stderr.push_str(&line);
            spawned.stderr.push('\n');
            if spawned.stderr.contains("listening on") {
                return true;
            }
        }
        if let Some(status) = spawned.child.try_wait().expect("try_wait") {
            while let Ok(line) = rx.try_recv() {
                spawned.stderr.push_str(&line);
                spawned.stderr.push('\n');
            }
            return status.success() && spawned.stderr.contains("listening on");
        }
        if start.elapsed() >= timeout {
            let _ = spawned.child.kill();
            while let Ok(line) = rx.try_recv() {
                spawned.stderr.push_str(&line);
                spawned.stderr.push('\n');
            }
            return spawned.stderr.contains("listening on");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_exit(spawned: &mut Spawned, timeout: Duration) -> (i32, String) {
    let start = Instant::now();
    let mut stderr = spawned.child.stderr.take().expect("runtime stderr");
    let handle = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });
    loop {
        if let Some(status) = spawned.child.try_wait().expect("try_wait") {
            let text = handle.join().unwrap_or_default();
            spawned.stderr = text.clone();
            return (status.code().unwrap_or(1), text);
        }
        if start.elapsed() >= timeout {
            let _ = spawned.child.kill();
            let _ = spawned.child.wait();
            let text = handle.join().unwrap_or_default();
            spawned.stderr = text.clone();
            return (1, text);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// F1: an empty file must be spawned and listen with no ceremony.
#[test]
fn empty_file_spawns_and_listens() {
    let socket = scratch("s.sock");
    let db = scratch("empty.db");
    let _ = std::fs::remove_file(&socket);
    std::fs::write(&db, []).expect("create empty file");
    let mut spawned = spawn_runtime(&socket, &db);
    let listening = wait_listen_or_exit(&mut spawned, TIMEOUT);
    assert!(
        listening,
        "empty file must listen; stderr={}",
        spawned.stderr
    );
}

/// F2: a marker-ed used DB must serve on a second spawn without double migrate.
#[test]
fn marked_used_db_serves_without_double_migrate() {
    let dir = scratch("used");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("used.db");
    let socket = scratch("used.sock");
    let _ = std::fs::remove_file(&socket);
    {
        let conn = Connection::open(&db).unwrap();
        opencrab_db::schema::initialize(&conn).unwrap();
        drop(conn);
        let (config, environment) = write_migration_inputs(&dir);
        let mut conn = Connection::open(&db).unwrap();
        migrate_in_place(&mut conn, &config, &environment, 1).unwrap();
        drop(conn);
    }
    {
        let mut first = spawn_runtime(&socket, &db);
        assert!(
            wait_listen_or_exit(&mut first, TIMEOUT),
            "first spawn of marked used DB must listen; stderr={}",
            first.stderr
        );
        let first_err = first.stderr.clone();
        drop(first);
        assert!(
            !first_err.contains("AlreadyApplied") || first_err.contains("listening on"),
            "first spawn must not fail loud on a marked DB: {first_err}"
        );
    }
    let mut second = spawn_runtime(&socket, &db);
    assert!(
        wait_listen_or_exit(&mut second, TIMEOUT),
        "second spawn of marked used DB must serve; stderr={}",
        second.stderr
    );
    assert!(
        !second.stderr.contains("AlreadyApplied"),
        "second spawn must be a marker no-op, not a double migrate: {}",
        second.stderr
    );
    let conn = Connection::open(&db).unwrap();
    let marked: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_migration_state WHERE migration_id=?1",
            [IN_PLACE_MIGRATION_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(marked, 1, "marker must remain a single row");
}

/// F3: legacy nonempty without a marker must refuse to serve.
#[test]
fn legacy_nonempty_without_marker_refuses() {
    let socket = scratch("legacy.sock");
    let db = scratch("legacy.db");
    let _ = std::fs::remove_file(&socket);
    {
        let conn = Connection::open(&db).unwrap();
        opencrab_db::schema::initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO agents(agent_id,name,persona_name) VALUES('synthetic-agent','A','p')",
            [],
        )
        .unwrap();
        drop(conn);
    }
    let mut spawned = spawn_runtime(&socket, &db);
    let (code, stderr) = wait_exit(&mut spawned, TIMEOUT);
    assert_ne!(code, 0, "legacy nonempty without marker must exit nonzero");
    assert!(
        stderr.contains("NeedsManualMigration") || stderr.contains("legacy tables are non-empty"),
        "refusal body must identify NeedsManualMigration; stderr={stderr}"
    );
}

/// F-shape (#784): a used new-structure DB (rows in events/subjects, no
/// schema_migration_state) must serve. Today `legacy_tables_nonempty` refuses.
#[test]
fn used_new_structure_db_without_marker_serves() {
    let socket = scratch("used-new.sock");
    let db = scratch("used-new.db");
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&db);
    {
        let store = Store::open(&db).expect("create used new-structure store");
        drop(store);
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO subjects(kind,name,persona,turn_runner,standing,created_at)
             VALUES('human','synthetic-used','','engine','owner',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events(place_id,seq,kind,content_json,mentions_json,created_at,attachments_json)
             VALUES(1,1,'said','{\"text\":\"synthetic-used-event\",\"symbol\":null}','[]',1,'[]')",
            [],
        )
        .unwrap();
        conn.execute("DROP TABLE schema_migration_state", [])
            .unwrap();
        let subjects: i64 = conn
            .query_row("SELECT COUNT(*) FROM subjects", [], |row| row.get(0))
            .unwrap();
        let events: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert!(subjects > 0 && events > 0, "used new-structure rows");
        let marker_table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migration_state'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker_table, 0, "schema_migration_state must be absent");
    }
    let mut spawned = spawn_runtime(&socket, &db);
    let listening = wait_listen_or_exit(&mut spawned, TIMEOUT);
    assert!(
        listening,
        "used new-structure DB without marker must serve; stderr={}",
        spawned.stderr
    );
}
