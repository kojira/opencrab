//! v66: `skills.tools` (D-1143).
//!
//! The tools a skill lists. When the owner has designated the skill as usable by anyone
//! (`agent_visible`), a turn that opens it with `read_skill` may call these tools whatever its
//! caller. Column only: every existing row starts with `[]`, so no existing skill grants anything
//! until the owner sets its tools from the dashboard or `PUT /api/agents/{id}/skills/{skill_id}`.
//! Nothing is copied from `situation_pattern` or the skill files: some of those already name tools
//! such as `execute_shell` on skills that were marked `agent_visible` for the older meaning
//! ("show to caller=Agent turns"), and copying them would grant tools the owner never chose.

use rusqlite::Connection;

use super::super::helpers::column_exists;
use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 66,
    description: "skills.tools: tools an owner-designated skill grants to any caller (D-1143)",
    up: add_skill_tools,
}];

fn add_skill_tools(conn: &Connection) -> rusqlite::Result<()> {
    if !column_exists(conn, "skills", "tools")? {
        conn.execute_batch("ALTER TABLE skills ADD COLUMN tools TEXT NOT NULL DEFAULT '[]'")?;
    }
    Ok(())
}
