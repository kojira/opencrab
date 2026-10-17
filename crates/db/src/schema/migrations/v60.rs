//! v60: per-session model override (D-1080).
//!
//! `sessions.model` is nullable. NULL means the session inherits the agent's model (and then
//! the server default); a non-empty canonical `provider:model` overrides it for every turn of
//! that session. Existing rows stay NULL: no backfill from `agents.model`.

use super::super::helpers::column_exists;
use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 60,
    description: "add nullable per-session model override",
    up: |conn| {
        if !column_exists(conn, "sessions", "model")? {
            conn.execute_batch("ALTER TABLE sessions ADD COLUMN model TEXT;")?;
        }
        Ok(())
    },
}];
