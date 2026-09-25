use anyhow::{bail, Result};
use rusqlite::{params, Connection, Transaction};

use super::TrustedUserPermission;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiPrincipalRow {
    pub id: String,
    pub user_id: String,
    pub agent_id: String,
    pub permission: String,
    pub created_by: String,
    pub created_at: String,
    pub display_name: String,
}

impl ApiPrincipalRow {
    pub fn parsed_permission(&self) -> TrustedUserPermission {
        TrustedUserPermission::from_db_str(&self.permission)
    }
}

const COLUMNS: &str = "id, user_id, agent_id, permission, created_by, created_at, display_name";

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiPrincipalRow> {
    Ok(ApiPrincipalRow {
        id: row.get(0)?,
        user_id: row.get(1)?,
        agent_id: row.get(2)?,
        permission: row.get(3)?,
        created_by: row.get(4)?,
        created_at: row.get(5)?,
        display_name: row.get(6)?,
    })
}

pub fn get_api_principal(
    conn: &Connection,
    user_id: &str,
    agent_id: &str,
) -> Option<ApiPrincipalRow> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM api_principals WHERE user_id=?1 AND agent_id=?2"),
        params![user_id, agent_id],
        from_row,
    )
    .ok()
}

/// Inserts one approved REST principal or accepts the byte-identical existing row.
/// Any primary-key or `(user_id,agent_id)` collision with different bytes fails.
pub fn insert_api_principal_in_tx(
    tx: &Transaction<'_>,
    principal: &ApiPrincipalRow,
) -> Result<bool> {
    let by_id = tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM api_principals WHERE id=?1"),
            [&principal.id],
            from_row,
        )
        .ok();
    let by_identity = tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM api_principals WHERE user_id=?1 AND agent_id=?2"),
            params![principal.user_id, principal.agent_id],
            from_row,
        )
        .ok();
    match (by_id, by_identity) {
        (None, None) => {
            tx.execute(
                "INSERT INTO api_principals
                 (id,user_id,agent_id,permission,created_by,created_at,display_name)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    principal.id,
                    principal.user_id,
                    principal.agent_id,
                    principal.permission,
                    principal.created_by,
                    principal.created_at,
                    principal.display_name,
                ],
            )?;
            Ok(true)
        }
        (Some(ref existing), Some(ref same)) if existing == principal && same == principal => {
            Ok(false)
        }
        _ => bail!("api_principal_conflict"),
    }
}
