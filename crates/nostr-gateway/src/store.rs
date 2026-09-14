//! Gateway-owned configuration store and one-shot legacy import.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use opencrab_db::queries::SessionWatchRow;
use opencrab_nostr::NostrGateAllowKeys;
use rusqlite::{params, Connection, OptionalExtension as _};

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS gateway_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS instances (
    agent_id TEXT PRIMARY KEY,
    agent_name TEXT NOT NULL,
    secret_key TEXT NOT NULL,
    relays_json TEXT NOT NULL,
    filter_json TEXT NOT NULL,
    enabled INTEGER NOT NULL,
    owner_pubkey TEXT NOT NULL DEFAULT '',
    self_pubkey TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS watches (
    watch_id INTEGER PRIMARY KEY,
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    interval_secs INTEGER NOT NULL,
    filter_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(agent_id, session_id)
);
CREATE TABLE IF NOT EXISTS allow_identities (
    agent_id TEXT NOT NULL,
    role TEXT NOT NULL,
    external_id TEXT NOT NULL,
    mapped_agent_id TEXT,
    PRIMARY KEY(agent_id, role, external_id)
);
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRow {
    pub agent_id: String,
    pub agent_name: String,
    pub secret_key: String,
    pub relays_json: String,
    pub filter_json: String,
    pub enabled: bool,
}

pub struct GatewayStore {
    path: PathBuf,
    conn: Connection,
}

impl GatewayStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create gateway DB parent {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("open gateway DB {}", path.display()))?;
        conn.execute_batch(SCHEMA)
            .context("initialize gateway DB")?;
        Ok(Self {
            path: path.to_path_buf(),
            conn,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn import_legacy_once(&mut self, legacy_path: &Path) -> Result<bool> {
        if self
            .conn
            .query_row(
                "SELECT value FROM gateway_meta WHERE key = 'legacy_import_v1'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .is_some()
        {
            return Ok(false);
        }
        let legacy =
            Connection::open_with_flags(legacy_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("open legacy DB {}", legacy_path.display()))?;
        let tx = self.conn.transaction()?;
        {
            let mut stmt = legacy.prepare(
                "SELECT c.agent_id, COALESCE(a.name, c.agent_id), c.secret_key,
                        c.relays_json, c.filter_json, c.enabled,
                        COALESCE(c.owner_pubkey, ''), COALESCE(c.self_pubkey, ''), c.updated_at
                 FROM agent_nostr_config c
                 LEFT JOIN agents a ON a.agent_id = c.agent_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            })?;
            for row in rows {
                let (agent_id, agent_name, secret, relays, filter, enabled, owner, own, updated) =
                    row?;
                tx.execute(
                    "INSERT INTO instances
                     (agent_id, agent_name, secret_key, relays_json, filter_json, enabled,
                      owner_pubkey, self_pubkey, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(agent_id) DO NOTHING",
                    params![
                        agent_id, agent_name, secret, relays, filter, enabled, owner, own, updated
                    ],
                )?;
            }
        }
        import_watches(&legacy, &tx)?;
        import_allow_identities(&legacy, &tx)?;
        tx.execute(
            "INSERT INTO gateway_meta (key, value) VALUES ('legacy_import_v1', ?1)",
            params![chrono::Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn get(&self, agent_id: &str) -> Result<Option<InstanceRow>> {
        self.conn
            .query_row(
                "SELECT agent_id, agent_name, secret_key, relays_json, filter_json, enabled
                 FROM instances WHERE agent_id = ?1",
                params![agent_id],
                |row| {
                    Ok(InstanceRow {
                        agent_id: row.get(0)?,
                        agent_name: row.get(1)?,
                        secret_key: row.get(2)?,
                        relays_json: row.get(3)?,
                        filter_json: row.get(4)?,
                        enabled: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn encrypt_plaintext_secrets(
        &mut self,
        master_key: &[u8; opencrab_core::secret_box::MASTER_KEY_LEN],
    ) -> Result<usize> {
        let rows = self.list_all_secret_values()?;
        let tx = self.conn.transaction()?;
        let mut changed = 0;
        for (agent_id, secret) in rows {
            if secret.trim().is_empty() || opencrab_core::secret_box::is_encrypted(&secret) {
                continue;
            }
            let encrypted = opencrab_core::secret_box::encrypt(secret.as_bytes(), master_key)?;
            changed += tx.execute(
                "UPDATE instances SET secret_key = ?1, updated_at = ?2 WHERE agent_id = ?3",
                params![encrypted, chrono::Utc::now().to_rfc3339(), agent_id],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    fn list_all_secret_values(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT agent_id, secret_key FROM instances")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn list_enabled(&self) -> Result<Vec<InstanceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT agent_id, agent_name, secret_key, relays_json, filter_json, enabled
             FROM instances WHERE enabled = 1 ORDER BY agent_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(InstanceRow {
                agent_id: row.get(0)?,
                agent_name: row.get(1)?,
                secret_key: row.get(2)?,
                relays_json: row.get(3)?,
                filter_json: row.get(4)?,
                enabled: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn upsert(
        &self,
        row: &InstanceRow,
        master_key: &[u8; opencrab_core::secret_box::MASTER_KEY_LEN],
    ) -> Result<()> {
        let secret = if opencrab_core::secret_box::is_encrypted(&row.secret_key) {
            row.secret_key.clone()
        } else {
            opencrab_core::secret_box::encrypt(row.secret_key.as_bytes(), master_key)?
        };
        self.conn.execute(
            "INSERT INTO instances
             (agent_id, agent_name, secret_key, relays_json, filter_json, enabled, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(agent_id) DO UPDATE SET
               agent_name = excluded.agent_name,
               secret_key = excluded.secret_key,
               relays_json = excluded.relays_json,
               filter_json = excluded.filter_json,
               enabled = excluded.enabled,
               updated_at = excluded.updated_at",
            params![
                row.agent_id,
                row.agent_name,
                secret,
                row.relays_json,
                row.filter_json,
                row.enabled,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn delete(&self, agent_id: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM instances WHERE agent_id = ?1",
            params![agent_id],
        )? > 0)
    }

    pub fn set_enabled(&self, agent_id: &str, enabled: bool) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET enabled = ?1, updated_at = ?2 WHERE agent_id = ?3",
            params![enabled, chrono::Utc::now().to_rfc3339(), agent_id],
        )? > 0)
    }

    pub fn set_self_pubkey(&self, agent_id: &str, value: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET self_pubkey = ?1, updated_at = ?2 WHERE agent_id = ?3",
            params![value, chrono::Utc::now().to_rfc3339(), agent_id],
        )? > 0)
    }

    pub fn watches(&self, agent_id: &str) -> Result<Vec<SessionWatchRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, interval_secs, filter_json
             FROM watches WHERE agent_id = ?1 ORDER BY watch_id",
        )?;
        let rows = stmt.query_map(params![agent_id], |row| {
            Ok(SessionWatchRow {
                id: 0,
                agent_id: agent_id.to_string(),
                session_id: row.get(0)?,
                interval_secs: row.get(1)?,
                filter_json: row.get(2)?,
                created_at: String::new(),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn allow_keys(&self, agent_id: &str) -> Result<NostrGateAllowKeys> {
        let owner = self.single_values(agent_id, "owner")?;
        let trusted_users = self.single_values(agent_id, "trusted")?;
        let mut co_agents = Vec::new();
        let mut co_agent_identities = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT external_id, mapped_agent_id FROM allow_identities
             WHERE agent_id = ?1 AND role = 'co_agent' ORDER BY external_id",
        )?;
        for row in stmt.query_map(params![agent_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (external_id, mapped_agent_id) = row?;
            co_agents.push(external_id.clone());
            co_agent_identities.push((external_id, mapped_agent_id));
        }
        Ok(NostrGateAllowKeys {
            owner,
            co_agents,
            co_agent_identities,
            trusted_users,
        })
    }

    fn single_values(&self, agent_id: &str, role: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT external_id FROM allow_identities
             WHERE agent_id = ?1 AND role = ?2 ORDER BY external_id",
        )?;
        let rows = stmt.query_map(params![agent_id, role], |row| row.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

fn import_watches(legacy: &Connection, tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let mut stmt = legacy.prepare(
        "SELECT id, agent_id, session_id, interval_secs, filter_json, created_at
         FROM session_watches",
    )?;
    for row in stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })? {
        let (id, agent, session, interval, filter, created) = row?;
        tx.execute(
            "INSERT OR IGNORE INTO watches
             (watch_id, agent_id, session_id, interval_secs, filter_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, agent, session, interval, filter, created],
        )?;
    }
    Ok(())
}

fn import_allow_identities(legacy: &Connection, tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let mut configs = legacy.prepare(
        "SELECT agent_id, owner_pubkey FROM agent_nostr_config WHERE owner_pubkey <> ''",
    )?;
    for row in configs.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (agent, external) = row?;
        tx.execute(
            "INSERT OR IGNORE INTO allow_identities (agent_id, role, external_id) VALUES (?1, 'owner', ?2)",
            params![agent, external],
        )?;
    }
    let mut trusted =
        legacy.prepare("SELECT agent_id, user_id FROM trusted_users WHERE platform = 'nostr'")?;
    for row in trusted.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (agent, external) = row?;
        tx.execute(
            "INSERT OR IGNORE INTO allow_identities (agent_id, role, external_id) VALUES (?1, 'trusted', ?2)",
            params![agent, external],
        )?;
    }
    let mut peers = legacy.prepare(
        "SELECT c.agent_id, p.self_pubkey, c.co_agent_id
         FROM trusted_co_agents c JOIN agent_nostr_config p ON p.agent_id = c.co_agent_id
         WHERE p.self_pubkey <> ''",
    )?;
    for row in peers.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (agent, external, mapped) = row?;
        tx.execute(
            "INSERT OR IGNORE INTO allow_identities
             (agent_id, role, external_id, mapped_agent_id) VALUES (?1, 'co_agent', ?2, ?3)",
            params![agent, external, mapped],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_fixture(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE agents (agent_id TEXT PRIMARY KEY, name TEXT);
             CREATE TABLE agent_nostr_config (
               agent_id TEXT PRIMARY KEY, secret_key TEXT, relays_json TEXT, filter_json TEXT,
               enabled INTEGER, owner_pubkey TEXT, self_pubkey TEXT, updated_at TEXT
             );
             CREATE TABLE session_watches (
               id INTEGER PRIMARY KEY, agent_id TEXT, session_id TEXT, interval_secs INTEGER,
               filter_json TEXT, created_at TEXT
             );
             CREATE TABLE trusted_users (agent_id TEXT, platform TEXT, user_id TEXT);
             CREATE TABLE trusted_co_agents (agent_id TEXT, co_agent_id TEXT);
             INSERT INTO agents VALUES ('a1', 'Agent One'), ('a2', 'Agent Two');
             INSERT INTO agent_nostr_config VALUES
               ('a1', 'nsec1plain', '[\"wss://relay.test\"]', '{}', 1, 'owner-key', 'self-one', 'now'),
               ('a2', 'nsec1peer', '[]', '{}', 0, '', 'self-two', 'now');
             INSERT INTO session_watches VALUES (7, 'a1', 'session-a1', 60, '{}', 'now');
             INSERT INTO trusted_users VALUES ('a1', 'nostr', 'trusted-key');
             INSERT INTO trusted_co_agents VALUES ('a1', 'a2');",
        )
        .unwrap();
    }

    #[test]
    fn legacy_import_is_one_shot_and_runtime_reads_gateway_store_only() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy.db");
        let gateway = dir.path().join("gateway.db");
        legacy_fixture(&legacy);

        let mut store = GatewayStore::open(&gateway).unwrap();
        assert!(store.import_legacy_once(&legacy).unwrap());
        assert!(!store.import_legacy_once(&legacy).unwrap());
        std::fs::remove_file(&legacy).unwrap();

        let rows = store.list_enabled().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_name, "Agent One");
        assert_eq!(store.watches("a1").unwrap()[0].session_id, "session-a1");
        let keys = store.allow_keys("a1").unwrap();
        assert_eq!(keys.owner, vec!["owner-key"]);
        assert_eq!(keys.trusted_users, vec!["trusted-key"]);
        assert_eq!(
            keys.co_agent_identities,
            vec![("self-two".to_string(), "a2".to_string())]
        );
    }

    #[test]
    fn plaintext_secrets_are_encrypted_and_reopenable() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy.db");
        let gateway = dir.path().join("gateway.db");
        legacy_fixture(&legacy);
        let mut store = GatewayStore::open(&gateway).unwrap();
        store.import_legacy_once(&legacy).unwrap();

        assert_eq!(store.encrypt_plaintext_secrets(&[9; 32]).unwrap(), 2);
        assert_eq!(store.encrypt_plaintext_secrets(&[9; 32]).unwrap(), 0);
        let secret = store.get("a1").unwrap().unwrap().secret_key;
        assert!(opencrab_core::secret_box::is_encrypted(&secret));
        let clear = opencrab_core::secret_box::decrypt(&secret, &[9; 32]).unwrap();
        assert_eq!(clear.as_slice(), b"nsec1plain");
    }
}
