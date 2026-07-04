pub mod queries;
pub mod schema;

use anyhow::Result;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

/// r2d2 コネクションプール（ファイルDB・本番用）。
pub type Pool = r2d2::Pool<SqliteConnectionManager>;
/// プールから借用した接続。
pub type PooledConn = r2d2::PooledConnection<SqliteConnectionManager>;

/// 接続ごとに適用する PRAGMA（`foreign_keys` は接続ローカルで永続しないため必須）。
///
/// `init_connection` とプール customizer の双方から共有する。`busy_timeout` は
/// プール化で初めて発生しうる同時ライタの `SQLITE_BUSY` を待機・再試行で吸収する。
fn configure_connection(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
}

/// DB ハンドルのエラー。
#[derive(Debug)]
pub enum DbError {
    /// プールからの接続取得に失敗（タイムアウト等）。
    Pool(r2d2::Error),
    /// Mutex が poison した（単一接続バリアントのみ）。
    Poisoned,
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Pool(e) => write!(f, "db pool error: {e}"),
            DbError::Poisoned => write!(f, "db connection mutex poisoned"),
        }
    }
}

impl std::error::Error for DbError {}

/// `Db::lock()` が返す接続ガード。`Deref<Target = Connection>` で `&Connection` として使える。
///
/// enum に `MutexGuard`（`!Send`）を含むため、このガード全体が `!Send`。これにより
/// プール利用時でも「接続を `.await` 跨ぎで保持する」ことがコンパイル時に禁止され、
/// 旧 `std::sync::Mutex` が与えていた構造的保護が維持される。
pub enum DbGuard<'a> {
    Pooled(PooledConn),
    Single(MutexGuard<'a, Connection>),
}

impl Deref for DbGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        match self {
            DbGuard::Pooled(c) => c,
            DbGuard::Single(g) => g,
        }
    }
}

impl DerefMut for DbGuard<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        match self {
            DbGuard::Pooled(c) => c,
            DbGuard::Single(g) => g,
        }
    }
}

#[derive(Clone)]
enum DbInner {
    /// 本番: r2d2 プール（並行読み取り可）。
    Pool(Pool),
    /// テスト/単一接続: 旧来の `Arc<Mutex<Connection>>`。
    Single(Arc<Mutex<Connection>>),
}

/// 共有DBハンドル。
///
/// `open()` は本番向けにコネクションプールを張り、`memory()` / `from_connection()` は
/// 単一接続（`Arc<Mutex<Connection>>`）を保持する。いずれも `lock()` で `DbGuard` を返し、
/// 呼び出し側は旧 `Arc<Mutex<Connection>>` と同じく `db.lock().unwrap()` 等で使える。
#[derive(Clone)]
pub struct Db {
    inner: DbInner,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.inner {
            DbInner::Pool(_) => "Pool",
            DbInner::Single(_) => "Single",
        };
        f.debug_struct("Db").field("inner", &kind).finish()
    }
}

impl Db {
    /// ファイルDBを開き、コネクションプール（本番用）を構築する。
    ///
    /// 全接続に PRAGMA（WAL/foreign_keys/busy_timeout）を適用し、スキーマ初期化を
    /// 1接続で1回だけ実行する（baseline の破壊的マイグレーションが複数接続で競合しない）。
    pub fn open(path: &str) -> Result<Db> {
        let path = Path::new(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let manager = SqliteConnectionManager::file(path).with_init(|c| configure_connection(c));
        let pool = r2d2::Pool::builder().max_size(8).build(manager)?;

        // スキーマ初期化は1接続で1回だけ、並行チェックアウト前に。
        {
            let conn = pool.get()?;
            schema::initialize(&conn)?;
        }

        Ok(Db {
            inner: DbInner::Pool(pool),
        })
    }

    /// インメモリDBのハンドル（主にテスト用）。単一接続を保持する。
    pub fn memory() -> Result<Db> {
        Ok(Db::from_connection(init_memory()?))
    }

    /// 既存の `Connection` から単一接続ハンドルを構築する。
    pub fn from_connection(conn: Connection) -> Db {
        Db {
            inner: DbInner::Single(Arc::new(Mutex::new(conn))),
        }
    }

    /// 接続を取得する。
    ///
    /// 命名は旧 `Arc<Mutex<Connection>>` とのドロップイン互換のため。戻り値の
    /// [`DbGuard`] は `Deref<Target = Connection>` なので `&*guard` / `&guard` を
    /// `queries::*(&Connection)` に渡せる。**`.await` を跨いで保持しないこと**
    /// （`DbGuard` は `!Send` なので跨ぐとコンパイルエラーになる）。
    pub fn lock(&self) -> std::result::Result<DbGuard<'_>, DbError> {
        match &self.inner {
            DbInner::Pool(pool) => pool.get().map(DbGuard::Pooled).map_err(DbError::Pool),
            DbInner::Single(m) => m.lock().map(DbGuard::Single).map_err(|_| DbError::Poisoned),
        }
    }
}

/// データベース接続を初期化（単一 `Connection`。CLI やテストの直接利用向け）。
pub fn init_connection(path: &str) -> Result<Connection> {
    let path = Path::new(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path)?;
    configure_connection(&conn)?;
    schema::initialize(&conn)?;

    Ok(conn)
}

/// インメモリDB（テスト用の生 `Connection`）。
pub fn init_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    schema::initialize(&conn)?;
    Ok(conn)
}

#[cfg(test)]
mod db_tests {
    use super::*;

    fn assert_send<T: Send>() {}
    fn assert_clone<T: Clone>() {}

    #[test]
    fn db_is_clone_send_sync() {
        assert_clone::<Db>();
        assert_send::<Db>();
        fn assert_sync<T: Sync>() {}
        assert_sync::<Db>();
    }

    #[test]
    fn memory_cross_checkout_visibility() {
        let db = Db::memory().unwrap();
        {
            let c = db.lock().unwrap();
            c.execute_batch(
                "INSERT INTO agents (agent_id, name, persona_name) VALUES ('a1','n','p')",
            )
            .unwrap();
        }
        let c = db.lock().unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM agents WHERE agent_id='a1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn file_open_pooled_and_schema_initialized_once() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "opencrab_db_pool_test_{}.sqlite",
            std::process::id()
        ));
        let path_str = path.to_str().unwrap();
        let _ = std::fs::remove_file(&path);

        let db = Db::open(path_str).unwrap();
        // foreign_keys が接続ごとに ON になっている（customizer 検証）。
        {
            let c = db.lock().unwrap();
            let fk: i64 = c
                .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
                .unwrap();
            assert_eq!(fk, 1);
            let uv: i64 = c
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert!(uv >= 1, "schema should be initialized");
        }
        // 2回目の open も冪等（schema init 再実行なし・エラーなし）。
        let db2 = Db::open(path_str).unwrap();
        {
            let c = db2.lock().unwrap();
            c.execute_batch(
                "INSERT INTO agents (agent_id, name, persona_name) VALUES ('x','n','p')",
            )
            .unwrap();
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    }

    #[test]
    fn file_pool_concurrency_smoke() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "opencrab_db_conc_test_{}.sqlite",
            std::process::id()
        ));
        let path_str = path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&path);

        let db = Db::open(&path_str).unwrap();
        let mut handles = Vec::new();
        for i in 0..16 {
            let db = db.clone();
            handles.push(std::thread::spawn(move || {
                let c = db.lock().unwrap();
                c.execute(
                    "INSERT INTO agents (agent_id, name, persona_name) VALUES (?1,'n','p')",
                    rusqlite::params![format!("agent-{i}")],
                )
                .unwrap();
                let _n: i64 = c
                    .query_row("SELECT COUNT(*) FROM agents", [], |r| r.get(0))
                    .unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let c = db.lock().unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM agents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            n, 16,
            "all concurrent writes must land (no SQLITE_BUSY loss)"
        );
        drop(c);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    }
}
