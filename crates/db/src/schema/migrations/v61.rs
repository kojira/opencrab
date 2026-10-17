//! v61: per-session realtime model slot (D-1081).
//!
//! `sessions.realtime_model` is nullable. NULL means realtime turns of the session follow the
//! session's default-profile model (session override > agent > server default); a non-empty
//! canonical `provider:model` is used for realtime turns only. No backfill.

use super::super::helpers::column_exists;
use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 61,
    description: "add nullable per-session realtime model slot",
    up: |conn| {
        if !column_exists(conn, "sessions", "realtime_model")? {
            conn.execute_batch("ALTER TABLE sessions ADD COLUMN realtime_model TEXT;")?;
        }
        Ok(())
    },
}];
