//! Nostr-owned durable configuration, policy, identity, credential, and lifecycle store.

use crate::secret_store;
use anyhow::{Context as _, Result};
use opencrab_process_supervisor::lifecycle::{LifecycleState, PersistedLifecycle};
use rusqlite::{params, Connection, OptionalExtension as _};
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

const SCHEMA: &str = r#"
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS instances (
  instance_id TEXT PRIMARY KEY,
  agent_id TEXT NOT NULL UNIQUE,
  subject_id INTEGER NOT NULL CHECK(subject_id > 0),
  config_b64 TEXT NOT NULL,
  addresses_json TEXT NOT NULL,
  credential_envelope TEXT NOT NULL,
  subject_grant_envelope TEXT,
  enabled INTEGER NOT NULL,
  desired_generation INTEGER NOT NULL,
  applied_generation INTEGER,
  lifecycle_state TEXT NOT NULL,
  core_revision INTEGER,
  core_digest TEXT,
  binding_inventory_json TEXT NOT NULL DEFAULT '[]',
  process_id INTEGER,
  process_nonce TEXT,
  failure_count INTEGER NOT NULL DEFAULT 0,
  retry_at_unix_ms INTEGER,
  last_exit TEXT,
  updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS endpoints (
  instance_id TEXT NOT NULL REFERENCES instances(instance_id) ON DELETE CASCADE,
  channel_id TEXT NOT NULL,
  guild_id TEXT,
  readable INTEGER NOT NULL,
  writable INTEGER NOT NULL,
  policy_json TEXT NOT NULL,
  PRIMARY KEY(instance_id, channel_id)
);
CREATE TABLE IF NOT EXISTS identity_projections (
  instance_id TEXT NOT NULL REFERENCES instances(instance_id) ON DELETE CASCADE,
  role TEXT NOT NULL,
  external_id TEXT NOT NULL,
  relationship_id TEXT,
  relationship_revision INTEGER,
  PRIMARY KEY(instance_id, role, external_id)
);
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
);
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRow {
    pub instance_id: String,
    pub agent_id: String,
    pub subject_id: i64,
    pub config_b64: String,
    pub addresses: Vec<String>,
    pub credential_envelope: String,
    pub subject_grant_envelope: Option<String>,
    pub enabled: bool,
    pub desired_generation: u64,
    pub applied_generation: Option<u64>,
    pub lifecycle_state: LifecycleState,
    pub core_revision: Option<u64>,
    pub core_digest: Option<String>,
    pub core_bindings: Vec<String>,
    pub process_id: Option<u32>,
    pub process_nonce: Option<String>,
    pub failure_count: u32,
    pub retry_at_unix_ms: Option<i64>,
}

impl InstanceRow {
    pub fn lifecycle(&self) -> PersistedLifecycle {
        PersistedLifecycle {
            desired_generation: self.desired_generation,
            applied_generation: self.applied_generation,
            enabled: self.enabled,
            state: self.lifecycle_state,
            retry_at_unix_ms: self.retry_at_unix_ms,
            process_nonce: self.process_nonce.clone(),
        }
    }
}

pub struct NostrStore {
    path: PathBuf,
    conn: Connection,
}

impl NostrStore {
    /// Offline S8 upgrades an old gateway database inside its destination transaction.
    pub fn initialize_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(SCHEMA)?;
        Ok(())
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::initialize_schema(&conn)?;
        Ok(Self {
            path: path.into(),
            conn,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_desired(
        &self,
        instance_id: &str,
        agent_id: &str,
        subject_id: i64,
        config_b64: &str,
        addresses: &[String],
        credential: &str,
        subject_grant: Option<&str>,
        enabled: bool,
        key: &[u8; secret_store::MASTER_KEY_LEN],
    ) -> Result<u64> {
        anyhow::ensure!(subject_id > 0, "subject_id must be positive");
        let existing = self.get(instance_id)?;
        let generation = existing
            .as_ref()
            .map_or(1, |row| row.desired_generation + 1);
        let envelope = if credential.is_empty() {
            existing
                .as_ref()
                .map(|row| row.credential_envelope.clone())
                .context("credential is required for a new instance")?
        } else {
            secret_store::encrypt(credential.as_bytes(), key)?
        };
        let grant_envelope = match subject_grant {
            Some(grant) if !grant.is_empty() => Some(secret_store::encrypt(grant.as_bytes(), key)?),
            _ => existing
                .as_ref()
                .and_then(|row| row.subject_grant_envelope.clone()),
        };
        self.conn.execute(
            "INSERT INTO instances
             (instance_id, agent_id, subject_id, config_b64, addresses_json, credential_envelope,
              subject_grant_envelope, enabled, desired_generation, applied_generation, lifecycle_state, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,NULL,'pending',?10)
             ON CONFLICT(instance_id) DO UPDATE SET
               agent_id=excluded.agent_id, subject_id=excluded.subject_id,
               config_b64=excluded.config_b64, addresses_json=excluded.addresses_json,
               credential_envelope=excluded.credential_envelope,
               subject_grant_envelope=excluded.subject_grant_envelope, enabled=excluded.enabled,
               desired_generation=excluded.desired_generation, lifecycle_state='pending',
               process_id=NULL, process_nonce=NULL, retry_at_unix_ms=NULL,
               updated_at=excluded.updated_at",
            params![instance_id, agent_id, subject_id, config_b64,
                serde_json::to_string(addresses)?, envelope, grant_envelope, enabled, generation,
                chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(generation)
    }

    pub fn get(&self, instance_id: &str) -> Result<Option<InstanceRow>> {
        self.conn.query_row(
            "SELECT instance_id,agent_id,subject_id,config_b64,addresses_json,credential_envelope,
                    subject_grant_envelope,enabled,desired_generation,applied_generation,lifecycle_state,core_revision,
                    core_digest,binding_inventory_json,process_id,process_nonce,failure_count,retry_at_unix_ms
             FROM instances WHERE instance_id=?1",
            params![instance_id],
            |row| {
                let state: String = row.get(10)?;
                let desired: i64 = row.get(8)?;
                let applied: Option<i64> = row.get(9)?;
                let revision: Option<i64> = row.get(11)?;
                let failures: i64 = row.get(16)?;
                Ok(InstanceRow {
                    instance_id: row.get(0)?, agent_id: row.get(1)?, subject_id: row.get(2)?,
                    config_b64: row.get(3)?,
                    addresses: serde_json::from_str(&row.get::<_, String>(4)?).unwrap_or_default(),
                    credential_envelope: row.get(5)?, subject_grant_envelope: row.get(6)?, enabled: row.get(7)?,
                    desired_generation: u64::try_from(desired).unwrap_or(0),
                    applied_generation: applied.and_then(|v| u64::try_from(v).ok()),
                    lifecycle_state: LifecycleState::from_str(&state).unwrap_or(LifecycleState::Error),
                    core_revision: revision.and_then(|v| u64::try_from(v).ok()),
                    core_digest: row.get(12)?,
                    core_bindings: serde_json::from_str(&row.get::<_, String>(13)?).unwrap_or_default(),
                    process_id: row.get::<_, Option<i64>>(14)?.and_then(|value| u32::try_from(value).ok()),
                    process_nonce: row.get(15)?,
                    failure_count: u32::try_from(failures).unwrap_or(u32::MAX),
                    retry_at_unix_ms: row.get(17)?,
                })
            },
        ).optional().map_err(Into::into)
    }

    pub fn retry_due_error(
        &self,
        instance_id: &str,
        generation: u64,
        now_unix_ms: i64,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET lifecycle_state='pending',
             process_id=NULL,process_nonce=NULL,retry_at_unix_ms=NULL
             WHERE instance_id=?1 AND desired_generation=?2 AND lifecycle_state='error'
               AND (retry_at_unix_ms IS NULL OR retry_at_unix_ms<=?3)",
            params![instance_id, generation, now_unix_ms],
        )? == 1)
    }

    pub fn list(&self) -> Result<Vec<InstanceRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT instance_id FROM instances ORDER BY instance_id")?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| self.get(&id)?.context("instance vanished"))
            .collect()
    }

    pub fn mark_provisioning(&self, instance_id: &str, generation: u64) -> Result<bool> {
        self.transition(instance_id, generation, "pending", "provisioning")
    }

    pub fn mark_verified(
        &self,
        instance_id: &str,
        generation: u64,
        revision: u64,
        digest: &str,
        bindings: &[String],
        enabled: bool,
    ) -> Result<bool> {
        let state = if enabled { "ready" } else { "disabled" };
        Ok(self.conn.execute(
            "UPDATE instances SET applied_generation=?2,lifecycle_state=?3,core_revision=?4,
             core_digest=?5,binding_inventory_json=?6,process_id=NULL,process_nonce=NULL,
             failure_count=0,retry_at_unix_ms=NULL,updated_at=?7
             WHERE instance_id=?1 AND desired_generation=?2 AND lifecycle_state='provisioning'
               AND enabled=?8",
            params![
                instance_id,
                generation,
                state,
                revision,
                digest,
                serde_json::to_string(bindings)?,
                chrono::Utc::now().to_rfc3339(),
                enabled
            ],
        )? == 1)
    }

    pub fn record_started(
        &self,
        instance_id: &str,
        generation: u64,
        pid: u32,
        nonce: &str,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET process_id=?3,process_nonce=?4,updated_at=?5
             WHERE instance_id=?1 AND desired_generation=?2 AND applied_generation=?2
             AND lifecycle_state='ready' AND enabled=1",
            params![
                instance_id,
                generation,
                pid,
                nonce,
                chrono::Utc::now().to_rfc3339()
            ],
        )? == 1)
    }

    pub fn mark_running(
        &self,
        instance_id: &str,
        generation: u64,
        pid: u32,
        nonce: &str,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET lifecycle_state='running',updated_at=?5
             WHERE instance_id=?1 AND desired_generation=?2
             AND applied_generation=?2 AND lifecycle_state='ready' AND enabled=1
             AND process_id=?3 AND process_nonce=?4",
            params![
                instance_id,
                generation,
                pid,
                nonce,
                chrono::Utc::now().to_rfc3339()
            ],
        )? == 1)
    }

    pub fn mark_pending(&self, instance_id: &str, generation: u64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET lifecycle_state='pending',process_id=NULL,process_nonce=NULL,
             updated_at=?3 WHERE instance_id=?1 AND desired_generation=?2",
            params![instance_id, generation, chrono::Utc::now().to_rfc3339()],
        )? == 1)
    }

    pub fn mark_error(
        &self,
        instance_id: &str,
        generation: u64,
        code: &str,
        retry_at: i64,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET lifecycle_state='error',process_id=NULL,process_nonce=NULL,
             failure_count=failure_count+1,retry_at_unix_ms=?4,last_exit=?3,updated_at=?5
             WHERE instance_id=?1 AND desired_generation=?2",
            params![
                instance_id,
                generation,
                code,
                retry_at,
                chrono::Utc::now().to_rfc3339()
            ],
        )? == 1)
    }

    pub fn decrypt_credential(
        &self,
        instance_id: &str,
        key: &[u8; 32],
    ) -> Result<zeroize::Zeroizing<Vec<u8>>> {
        let row = self.get(instance_id)?.context("instance unknown")?;
        secret_store::decrypt(&row.credential_envelope, key)
    }

    pub fn decrypt_subject_grant(
        &self,
        instance_id: &str,
        key: &[u8; 32],
    ) -> Result<Option<zeroize::Zeroizing<Vec<u8>>>> {
        let row = self.get(instance_id)?.context("instance unknown")?;
        row.subject_grant_envelope
            .as_deref()
            .map(|value| secret_store::decrypt(value, key))
            .transpose()
    }

    pub fn clear_subject_grant(&self, instance_id: &str, generation: u64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET subject_grant_envelope=NULL WHERE instance_id=?1 AND desired_generation=?2",
            params![instance_id, generation],
        )? == 1)
    }

    fn transition(&self, id: &str, generation: u64, from: &str, to: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET lifecycle_state=?4,updated_at=?5
             WHERE instance_id=?1 AND desired_generation=?2 AND lifecycle_state=?3",
            params![id, generation, from, to, chrono::Utc::now().to_rfc3339()],
        )? == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s5_nostr_store_encrypts_secret_and_persists_lifecycle_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nostr.db");
        let key = [7_u8; 32];
        let store = NostrStore::open(&path).unwrap();
        let generation = store
            .upsert_desired(
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "agent",
                7,
                "Y29uZmln",
                &["opaque-address".into()],
                "token-secret",
                Some("grant-secret-at-rest"),
                true,
                &key,
            )
            .unwrap();
        let row = store
            .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap()
            .unwrap();
        assert_eq!(row.lifecycle_state, LifecycleState::Pending);
        assert!(!row.credential_envelope.contains("token-secret"));
        assert!(store
            .mark_provisioning(&row.instance_id, generation)
            .unwrap());
        assert!(store
            .mark_verified(
                &row.instance_id,
                generation,
                3,
                "digest",
                &["binding".into()],
                true
            )
            .unwrap());
        drop(store);
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes
            .windows(b"token-secret".len())
            .any(|window| window == b"token-secret"));
        assert!(!bytes
            .windows(b"grant-secret-at-rest".len())
            .any(|window| window == b"grant-secret-at-rest"));
        let reopened = NostrStore::open(&path).unwrap();
        let row = reopened
            .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap()
            .unwrap();
        assert!(row.lifecycle().child_may_start());
        assert_eq!(
            &*reopened.decrypt_credential(&row.instance_id, &key).unwrap(),
            b"token-secret"
        );
    }

    #[test]
    fn s5_nostr_disabled_and_nonready_instances_never_eligible() {
        let temp = tempfile::tempdir().unwrap();
        let store = NostrStore::open(&temp.path().join("nostr.db")).unwrap();
        let generation = store
            .upsert_desired(
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "agent",
                7,
                "Y29uZmln",
                &[],
                "token",
                None,
                false,
                &[8; 32],
            )
            .unwrap();
        let pending = store
            .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap()
            .unwrap();
        assert!(!pending.lifecycle().child_may_start());
        store
            .mark_provisioning(&pending.instance_id, generation)
            .unwrap();
        store
            .mark_verified(&pending.instance_id, generation, 2, "digest", &[], false)
            .unwrap();
        let disabled = store.get(&pending.instance_id).unwrap().unwrap();
        assert_eq!(disabled.lifecycle_state, LifecycleState::Disabled);
        assert!(!disabled.lifecycle().child_may_start());
    }
}
