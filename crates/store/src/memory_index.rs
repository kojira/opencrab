//! B 表 `memory_index_*` / `daily_log_index_*`（DESIGN-DASHBOARD-P2 SLICE 5）。
//! 旧表が唯一の家。LLM が要る構築は mock 先——未索引の実データがあるのに
//! 要約できないときは fail loud（0 件成功で取り繕わない）。

use crate::Store;
use opencrab_port::SubjectId;
use rusqlite::{params, OptionalExtension};

pub const BATCH_SIZE_MIN: i64 = 10;
pub const THRESHOLD_MIN: i64 = 5;
pub const BATCH_SIZE_DEFAULT: i64 = 50;
pub const THRESHOLD_DEFAULT: i64 = 20;

#[derive(Debug)]
pub enum MemoryIndexError {
    Store(rusqlite::Error),
    LlmRequired(&'static str),
}

impl From<rusqlite::Error> for MemoryIndexError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

impl std::fmt::Display for MemoryIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "{error}"),
            Self::LlmRequired(detail) => write!(f, "{detail}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryIndexConfig {
    pub agent_id: String,
    pub batch_size: i64,
    pub threshold: i64,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryIndexBuildResult {
    pub nodes_created: i64,
    pub logs_indexed: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryIndexMergeResult {
    pub periods_processed: i64,
    pub topics_merged: i64,
    pub topics_deleted: i64,
}

fn agent_key(subject: SubjectId) -> String {
    subject.to_string()
}

fn table_exists(conn: &rusqlite::Connection, table: &str) -> crate::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        params![table],
        |row| row.get(0),
    )
}

fn count_eq(conn: &rusqlite::Connection, sql: &str, agent: &str) -> crate::Result<i64> {
    conn.query_row(sql, params![agent], |row| row.get(0))
}

impl Store {
    pub fn memory_index_clear(
        &self,
        subject: SubjectId,
    ) -> std::result::Result<(), MemoryIndexError> {
        let agent = agent_key(subject);
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM memory_index_nodes WHERE agent_id=?1",
            params![agent],
        )?;
        if table_exists(&tx, "memory_index_fts")? {
            tx.execute(
                "DELETE FROM memory_index_fts WHERE agent_id=?1",
                params![agent],
            )?;
        }
        tx.execute(
            "DELETE FROM memory_index_watermark WHERE agent_id=?1",
            params![agent],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn memory_index_policy_update(
        &self,
        subject: SubjectId,
        batch_size: Option<i64>,
        threshold: Option<i64>,
        now: i64,
    ) -> std::result::Result<MemoryIndexConfig, MemoryIndexError> {
        let agent = agent_key(subject);
        let conn = self.c();
        let current = conn
            .query_row(
                "SELECT batch_size,threshold FROM agent_memory_index_config WHERE agent_id=?1",
                params![&agent],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let (cur_batch, cur_threshold) = current.unwrap_or((BATCH_SIZE_DEFAULT, THRESHOLD_DEFAULT));
        let batch_size = batch_size.unwrap_or(cur_batch).max(BATCH_SIZE_MIN);
        let threshold = threshold.unwrap_or(cur_threshold).max(THRESHOLD_MIN);
        let updated_at = now.to_string();
        conn.execute(
            "INSERT INTO agent_memory_index_config (agent_id, batch_size, threshold, updated_at)
             VALUES(?1,?2,?3,?4)
             ON CONFLICT(agent_id) DO UPDATE SET
               batch_size=excluded.batch_size,
               threshold=excluded.threshold,
               updated_at=excluded.updated_at",
            params![&agent, batch_size, threshold, &updated_at],
        )?;
        Ok(MemoryIndexConfig {
            agent_id: agent,
            batch_size,
            threshold,
            updated_at,
        })
    }

    pub fn memory_index_build(
        &self,
        subject: SubjectId,
    ) -> std::result::Result<MemoryIndexBuildResult, MemoryIndexError> {
        let agent = agent_key(subject);
        let conn = self.c();
        if table_exists(&conn, "session_logs")? {
            let pending = count_eq(
                &conn,
                "SELECT COUNT(*) FROM session_logs WHERE agent_id=?1",
                &agent,
            )?;
            if pending > 0 {
                return Err(MemoryIndexError::LlmRequired(
                    "memory-index.build needs LLM to summarize session_logs",
                ));
            }
        }
        Ok(MemoryIndexBuildResult {
            nodes_created: 0,
            logs_indexed: 0,
        })
    }

    pub fn memory_index_rebuild(
        &self,
        subject: SubjectId,
    ) -> std::result::Result<MemoryIndexBuildResult, MemoryIndexError> {
        self.memory_index_clear(subject)?;
        match self.memory_index_build(subject) {
            Ok(result) => Ok(result),
            Err(error) => {
                let _ = self.memory_index_clear(subject);
                Err(error)
            }
        }
    }

    pub fn memory_index_merge(
        &self,
        subject: SubjectId,
    ) -> std::result::Result<MemoryIndexMergeResult, MemoryIndexError> {
        let agent = agent_key(subject);
        let conn = self.c();
        let topics = count_eq(
            &conn,
            "SELECT COUNT(*) FROM memory_index_nodes WHERE agent_id=?1 AND node_type='topic'",
            &agent,
        )?;
        if topics > 0 {
            return Err(MemoryIndexError::LlmRequired(
                "memory-index.merge needs LLM to re-summarize topics",
            ));
        }
        Ok(MemoryIndexMergeResult {
            periods_processed: 0,
            topics_merged: 0,
            topics_deleted: 0,
        })
    }

    pub fn daily_log_index_rebuild(
        &self,
        subject: SubjectId,
    ) -> std::result::Result<(), MemoryIndexError> {
        self.daily_log_require_no_pending(subject)?;
        Ok(())
    }

    pub fn daily_log_index_run(
        &self,
        subject: SubjectId,
    ) -> std::result::Result<(), MemoryIndexError> {
        self.daily_log_require_no_pending(subject)?;
        Ok(())
    }

    fn daily_log_require_no_pending(
        &self,
        subject: SubjectId,
    ) -> std::result::Result<(), MemoryIndexError> {
        let agent = agent_key(subject);
        let conn = self.c();
        if table_exists(&conn, "memory_curated")? {
            let pending = count_eq(
                &conn,
                "SELECT COUNT(*) FROM memory_curated
                 WHERE agent_id=?1 AND category LIKE 'daily_log/%'",
                &agent,
            )?;
            if pending > 0 {
                return Err(MemoryIndexError::LlmRequired(
                    "daily-log-index needs LLM to summarize daily logs",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_port::{Standing, SubjectKind};

    fn store_with_agent() -> (Store, SubjectId) {
        let store = Store::new_in_memory().unwrap();
        let id = store
            .create_subject(
                SubjectKind::Agent,
                "Ada",
                "You are Ada.",
                "engine",
                Standing::Trusted,
                1,
            )
            .unwrap();
        (store, id)
    }

    #[test]
    fn clear_deletes_index_rows_and_watermark() {
        let (store, owner) = store_with_agent();
        let agent = owner.to_string();
        store
            .c()
            .execute(
                "INSERT INTO memory_index_nodes(
                   id,agent_id,node_type,title,summary,created_at,updated_at
                 ) VALUES('n1',?1,'root','root','s','t','t')",
                params![&agent],
            )
            .unwrap();
        store
            .c()
            .execute(
                "INSERT INTO memory_index_watermark(agent_id,last_indexed_log_id,last_indexed_at,total_nodes)
                 VALUES(?1,3,'t',1)",
                params![&agent],
            )
            .unwrap();
        store.memory_index_clear(owner).unwrap();
        let nodes: i64 = store
            .c()
            .query_row(
                "SELECT COUNT(*) FROM memory_index_nodes WHERE agent_id=?1",
                params![&agent],
                |row| row.get(0),
            )
            .unwrap();
        let marks: i64 = store
            .c()
            .query_row(
                "SELECT COUNT(*) FROM memory_index_watermark WHERE agent_id=?1",
                params![&agent],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(nodes, 0);
        assert_eq!(marks, 0);
    }

    #[test]
    fn policy_update_omits_keep_and_clamps() {
        let (store, owner) = store_with_agent();
        let first = store
            .memory_index_policy_update(owner, Some(80), None, 10)
            .unwrap();
        assert_eq!(first.batch_size, 80);
        assert_eq!(first.threshold, THRESHOLD_DEFAULT);
        let second = store
            .memory_index_policy_update(owner, Some(3), Some(1), 11)
            .unwrap();
        assert_eq!(second.batch_size, BATCH_SIZE_MIN);
        assert_eq!(second.threshold, THRESHOLD_MIN);
        let third = store
            .memory_index_policy_update(owner, None, None, 12)
            .unwrap();
        assert_eq!(third.batch_size, BATCH_SIZE_MIN);
        assert_eq!(third.threshold, THRESHOLD_MIN);
    }

    #[test]
    fn empty_store_build_merge_daily_are_zero() {
        let (store, owner) = store_with_agent();
        let built = store.memory_index_build(owner).unwrap();
        assert_eq!(
            built,
            MemoryIndexBuildResult {
                nodes_created: 0,
                logs_indexed: 0
            }
        );
        let rebuilt = store.memory_index_rebuild(owner).unwrap();
        assert_eq!(rebuilt.logs_indexed, 0);
        let merged = store.memory_index_merge(owner).unwrap();
        assert_eq!(merged.topics_merged, 0);
        store.daily_log_index_run(owner).unwrap();
        store.daily_log_index_rebuild(owner).unwrap();
    }

    #[test]
    fn merge_with_topics_fails_loud_without_llm() {
        let (store, owner) = store_with_agent();
        store
            .c()
            .execute(
                "INSERT INTO memory_index_nodes(
                   id,agent_id,node_type,title,summary,created_at,updated_at
                 ) VALUES('t1',?1,'topic','t','s','t','t')",
                params![owner.to_string()],
            )
            .unwrap();
        let err = store.memory_index_merge(owner).err().unwrap();
        assert!(matches!(err, MemoryIndexError::LlmRequired(_)));
    }
}
