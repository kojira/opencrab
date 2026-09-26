use anyhow::{bail, Result};
use rusqlite::{params, Connection, Transaction};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TrustedUserPermission {
    /// オーナー相当。
    Owner,
    /// ただの信頼済みユーザー（登録の既定）。
    #[default]
    User,
    /// 協働エージェント（相互レビューの名簿に載る）。
    CoAgent,
}

/// 権限の全体。ダッシュボードの選択肢はここから導く（一致は
/// `dashboard_permission_options_match_the_enum` が検査する）。
pub const TRUSTED_USER_PERMISSIONS: [TrustedUserPermission; 3] = [
    TrustedUserPermission::Owner,
    TrustedUserPermission::User,
    TrustedUserPermission::CoAgent,
];

impl TrustedUserPermission {
    /// DB に入る表記（ケバブケース）。**書き込みはこの関数だけを通す。**
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::User => "user",
            Self::CoAgent => "co-agent",
        }
    }

    /// 既知の表記だけを受け付ける（入口の検証用）。**別表記の受け入れはしない** —
    /// 「どちらの表記も通す」が #234 そのものだったので、通す表記はひとつに保つ。
    pub fn parse(s: &str) -> Option<Self> {
        TRUSTED_USER_PERMISSIONS
            .into_iter()
            .find(|p| p.as_db_str() == s)
    }

    /// DB の既存値を読む。**未知の値は [`Self::User`] へ倒す**（fail-closed）。
    ///
    /// 行があるのに権限が読めない状態は、従来も「協働エージェントでもオーナーでも
    /// ない＝ただの信頼済みユーザー」に落ちていた。判定結果を変えないため、その
    /// 落とし先を保つ（緩む方向へは倒さない）。
    pub fn from_db_str(s: &str) -> Self {
        Self::parse(s).unwrap_or(Self::User)
    }
}

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
