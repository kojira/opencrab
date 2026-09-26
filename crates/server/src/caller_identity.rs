//! 共有HTTP経路の呼び出し元権限判定。
//!
//! server は外部gateway固有の識別子空間やowner設定を解釈しない。認証済みgatewayは
//! 判定済みのcallerをgeneric wireで渡し、共有経路はopaqueなsourceとidentifierだけを扱う。

use opencrab_actions::CallerIdentity;

/// REST bodyの自己申告識別子をowner相当に昇格させないfail-closed判定。
pub fn resolve_rest_caller_identity(
    conn: &rusqlite::Connection,
    user_id: &str,
    agent_id: &str,
) -> CallerIdentity {
    resolve_rest_api_principal(
        opencrab_db::queries::get_api_principal(conn, user_id, agent_id),
        user_id,
    )
}

/// Preserve the historical REST permission mapping after the storage cutover.
/// REST `user_id` remains self-asserted, so owner-equivalent permissions are
/// deliberately downgraded to `TrustedUser` exactly as before S8.
pub fn resolve_rest_api_principal(
    principal: Option<opencrab_db::queries::ApiPrincipalRow>,
    user_id: &str,
) -> CallerIdentity {
    use opencrab_db::queries::ApiPrincipalPermission;

    let identity = match principal.map(|row| row.parsed_permission()) {
        Some(ApiPrincipalPermission::CoAgent) => CallerIdentity::CoAgent {
            agent_id: user_id.to_string(),
        },
        Some(ApiPrincipalPermission::Owner | ApiPrincipalPermission::User) => {
            CallerIdentity::TrustedUser
        }
        None => CallerIdentity::Agent,
    };
    if identity.is_owner_equivalent() {
        CallerIdentity::TrustedUser
    } else {
        identity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn register_api_principal(conn: &mut rusqlite::Connection, permission: &str) {
        let tx = conn.transaction().unwrap();
        opencrab_db::queries::insert_api_principal_in_tx(
            &tx,
            &opencrab_db::queries::ApiPrincipalRow {
                id: format!("api-{permission}"),
                user_id: permission.to_string(),
                agent_id: "agent-1".into(),
                permission: permission.to_string(),
                created_by: "owner".into(),
                created_at: "2026-01-01".into(),
                display_name: String::new(),
            },
        )
        .unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn rest_api_principal_preserves_fail_closed_permission_mapping() {
        let mut conn = opencrab_db::init_memory().unwrap();
        for permission in ["owner", "co-agent", "unknown", "user"] {
            register_api_principal(&mut conn, permission);
            assert_eq!(
                resolve_rest_caller_identity(&conn, permission, "agent-1"),
                CallerIdentity::TrustedUser,
                "permission={permission}"
            );
        }
        assert_eq!(
            resolve_rest_caller_identity(&conn, "missing", "agent-1"),
            CallerIdentity::Agent
        );
    }
}
