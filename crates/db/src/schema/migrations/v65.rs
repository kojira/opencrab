//! v65: drop `pending_interactions` (D-1099).
//!
//! The table held A2UI surfaces a transport rendered (`send_ui`) with a platform channel id. No
//! gateway renders A2UI and core no longer has `send_ui`, so nothing writes or reads it.

use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 65,
    description: "drop pending_interactions (D-1099)",
    up: |conn| conn.execute_batch("DROP TABLE IF EXISTS pending_interactions;"),
}];
