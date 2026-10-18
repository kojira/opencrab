//! v62: per-session reasoning effort slots (D-1086).
//!
//! `sessions.reasoning_effort` and `sessions.realtime_reasoning_effort` are nullable. NULL means
//! no session override: default-profile turns follow the agent's effort and then the provider
//! configuration; realtime-profile turns follow the provider configuration. No backfill.

use super::super::helpers::column_exists;
use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 62,
    description: "add nullable per-session reasoning effort slots",
    up: |conn| {
        for column in ["reasoning_effort", "realtime_reasoning_effort"] {
            if !column_exists(conn, "sessions", column)? {
                conn.execute_batch(&format!("ALTER TABLE sessions ADD COLUMN {column} TEXT;"))?;
            }
        }
        Ok(())
    },
}];
