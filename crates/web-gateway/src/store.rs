//! Web-owned durable instance, identity, policy, and credential store.

use crate::secret_store;
use anyhow::{Context as _, Result};
use rusqlite::{params, Connection, OptionalExtension as _};
use std::path::{Path, PathBuf};

const SCHEMA: &str = r#"
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS gateway_settings (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  http_bind TEXT NOT NULL,
  core_socket TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS instances (
  instance_id TEXT PRIMARY KEY,
  agent_id TEXT NOT NULL UNIQUE,
  revision INTEGER NOT NULL CHECK(revision > 0),
  author_id TEXT NOT NULL,
  credential_envelope TEXT,
  enabled INTEGER NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS identity_projections (
  instance_id TEXT NOT NULL REFERENCES instances(instance_id) ON DELETE CASCADE,
  role TEXT NOT NULL,
  external_id TEXT NOT NULL,
  relationship_id TEXT,
  relationship_revision INTEGER,
  PRIMARY KEY(instance_id, role,external_id)
);
CREATE TABLE IF NOT EXISTS policies (
  instance_id TEXT NOT NULL REFERENCES instances(instance_id) ON DELETE CASCADE,
  policy_key TEXT NOT NULL,
  policy_json TEXT NOT NULL,
  PRIMARY KEY(instance_id,policy_key)
);
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebInstance {
    pub instance_id: String,
    pub agent_id: String,
    pub revision: u64,
    pub author_id: String,
    pub credential_envelope: Option<String>,
    pub enabled: bool,
}

pub struct WebStore {
    path: PathBuf,
    conn: Connection,
}

impl WebStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            path: path.into(),
            conn,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn configure(&self, http_bind: &str, core_socket: &str) -> Result<()> {
        anyhow::ensure!(core_socket.starts_with('/'), "core_socket must be absolute");
        let bind: std::net::SocketAddr = http_bind.parse().context("http_bind invalid")?;
        anyhow::ensure!(bind.ip().is_loopback(), "http_bind must be loopback");
        self.conn.execute(
            "INSERT INTO gateway_settings VALUES (1,?1,?2)
             ON CONFLICT(singleton) DO UPDATE SET http_bind=excluded.http_bind,core_socket=excluded.core_socket",
            params![http_bind, core_socket],
        )?;
        Ok(())
    }

    pub fn settings(&self) -> Result<(String, String)> {
        self.conn
            .query_row(
                "SELECT http_bind,core_socket FROM gateway_settings WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(Into::into)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert(
        &self,
        instance_id: &str,
        agent_id: &str,
        revision: u64,
        author_id: &str,
        credential: Option<&str>,
        enabled: bool,
        key: &[u8; 32],
    ) -> Result<()> {
        anyhow::ensure!(revision > 0, "revision must be positive");
        let existing = self.get(instance_id)?;
        let envelope = match credential {
            Some(value) if !value.is_empty() => Some(secret_store::encrypt(value.as_bytes(), key)?),
            _ => existing.and_then(|row| row.credential_envelope),
        };
        self.conn.execute(
            "INSERT INTO instances VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(instance_id) DO UPDATE SET agent_id=excluded.agent_id,
             revision=excluded.revision,author_id=excluded.author_id,
             credential_envelope=excluded.credential_envelope,enabled=excluded.enabled,
             updated_at=excluded.updated_at",
            params![
                instance_id,
                agent_id,
                revision,
                author_id,
                envelope,
                enabled,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn set_caller_role(&self, instance_id: &str, role: &str) -> Result<()> {
        anyhow::ensure!(
            matches!(role, "owner" | "trusted_user"),
            "unsupported caller role"
        );
        self.conn.execute(
            "INSERT INTO identity_projections(instance_id,role,external_id) VALUES (?1,?2,'bearer')
             ON CONFLICT(instance_id,role,external_id) DO NOTHING",
            params![instance_id, role],
        )?;
        self.conn.execute(
            "DELETE FROM identity_projections WHERE instance_id=?1 AND external_id='bearer' AND role<>?2",
            params![instance_id, role],
        )?;
        Ok(())
    }

    pub fn caller_role(&self, instance_id: &str) -> Result<String> {
        self.conn
            .query_row(
                "SELECT role FROM identity_projections WHERE instance_id=?1 AND external_id='bearer'",
                [instance_id],
                |row| row.get(0),
            )
            .context("bearer caller policy missing")
    }

    pub fn decrypt_credential(
        &self,
        instance_id: &str,
        key: &[u8; 32],
    ) -> Result<zeroize::Zeroizing<Vec<u8>>> {
        let envelope = self
            .get(instance_id)?
            .and_then(|row| row.credential_envelope)
            .context("web credential missing")?;
        secret_store::decrypt(&envelope, key)
    }

    pub fn get(&self, instance_id: &str) -> Result<Option<WebInstance>> {
        self.conn.query_row(
            "SELECT instance_id,agent_id,revision,author_id,credential_envelope,enabled FROM instances WHERE instance_id=?1",
            params![instance_id], |row| {
                let revision: i64 = row.get(2)?;
                Ok(WebInstance { instance_id: row.get(0)?,agent_id: row.get(1)?,revision:u64::try_from(revision).unwrap_or(0),author_id:row.get(3)?,credential_envelope:row.get(4)?,enabled:row.get(5)? })
            }
        ).optional().map_err(Into::into)
    }

    pub fn list_enabled(&self) -> Result<Vec<WebInstance>> {
        let mut stmt = self
            .conn
            .prepare("SELECT instance_id FROM instances WHERE enabled=1 ORDER BY instance_id")?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| self.get(&id)?.context("instance vanished"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s5_web_store_rejects_non_loopback_bind() {
        let temp = tempfile::tempdir().unwrap();
        let store = WebStore::open(&temp.path().join("web.db")).unwrap();
        assert!(store
            .configure("0.0.0.0:8080", "/tmp/runtime.sock")
            .is_err());
    }

    #[test]
    fn s5_web_store_owns_identity_policy_and_redacted_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("web.db");
        let store = WebStore::open(&path).unwrap();
        store.configure("127.0.0.1:0", "/tmp/runtime.sock").unwrap();
        store
            .upsert(
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "agent",
                1,
                "author",
                Some("secret"),
                true,
                &[2; 32],
            )
            .unwrap();
        let row = store
            .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap()
            .unwrap();
        assert!(!row.credential_envelope.unwrap().contains("secret"));
        drop(store);
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes
            .windows(b"secret".len())
            .any(|window| window == b"secret"));
        let reopened = WebStore::open(&path).unwrap();
        assert_eq!(reopened.list_enabled().unwrap().len(), 1);
        assert_eq!(reopened.settings().unwrap().1, "/tmp/runtime.sock");
    }

    #[test]
    fn s5_web_agent_collision_is_rejected_and_identical_rerun_converges() {
        let temp = tempfile::tempdir().unwrap();
        let store = WebStore::open(&temp.path().join("web.db")).unwrap();
        store
            .upsert(
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "agent",
                1,
                "author",
                None,
                true,
                &[2; 32],
            )
            .unwrap();
        store
            .upsert(
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "agent",
                1,
                "author",
                None,
                true,
                &[2; 32],
            )
            .unwrap();
        assert!(store
            .upsert(
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "agent",
                1,
                "other",
                None,
                true,
                &[2; 32]
            )
            .is_err());
        assert_eq!(store.list_enabled().unwrap().len(), 1);
    }
}
