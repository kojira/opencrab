//! 共有HTTP経路の呼び出し元権限判定。
//!
//! server は外部gateway固有の識別子空間やowner設定を解釈しない。認証済みgatewayは
//! 判定済みのcallerをgeneric wireで渡し、共有経路はopaqueなsourceとidentifierだけを扱う。

use opencrab_actions::CallerIdentity;

/// owner情報を持たない共有経路で、source固有のtrusted-user行だけを参照する。
pub fn resolve_caller_identity(
    conn: &rusqlite::Connection,
    source: &str,
    user_id: &str,
    agent_id: &str,
) -> CallerIdentity {
    resolve_caller_identity_with_owner(conn, source, &[user_id], agent_id, "")
}

/// REST bodyの自己申告識別子をowner相当に昇格させないfail-closed判定。
pub fn resolve_rest_caller_identity(
    conn: &rusqlite::Connection,
    user_id: &str,
    agent_id: &str,
) -> CallerIdentity {
    let identity = resolve_caller_identity_with_owner(
        conn,
        opencrab_db::queries::TRUSTED_PLATFORM_REST,
        &[user_id],
        agent_id,
        "",
    );
    if identity.is_owner_equivalent() {
        CallerIdentity::TrustedUser
    } else {
        identity
    }
}

/// 認証済み境界が与えたopaqueなsource、identifier、owner識別子から権限を導出する。
///
/// serverは値の形式やsourceの種類を解釈しない。外部gatewayがこのcallbackを利用する場合も、
/// 署名・token等で認証済みのidentifierだけを渡すこと。
pub fn resolve_caller_identity_with_owner(
    conn: &rusqlite::Connection,
    source: &str,
    user_ids: &[&str],
    agent_id: &str,
    owner_id: &str,
) -> CallerIdentity {
    use opencrab_db::queries::{get_trusted_user, TrustedUserPermission};

    if user_ids
        .iter()
        .any(|user_id| opencrab_core::owner::is_owner_id(owner_id, user_id))
    {
        return CallerIdentity::Owner;
    }
    match user_ids.iter().find_map(|user_id| {
        get_trusted_user(conn, source, user_id, agent_id).map(|row| row.permission)
    }) {
        Some(TrustedUserPermission::CoAgent) => CallerIdentity::CoAgent {
            agent_id: user_ids.first().copied().unwrap_or_default().to_string(),
        },
        Some(TrustedUserPermission::Owner | TrustedUserPermission::User) => {
            CallerIdentity::TrustedUser
        }
        None => CallerIdentity::Agent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_db::queries::{
        TrustedUserPermission, TRUSTED_PLATFORM_REST, TRUSTED_PLATFORM_WEB,
    };

    fn register(
        conn: &rusqlite::Connection,
        source: &str,
        user_id: &str,
        permission: TrustedUserPermission,
    ) {
        opencrab_db::queries::add_trusted_user(
            conn,
            source,
            &format!("row-{source}-{user_id}"),
            "agent-1",
            user_id,
            permission,
            "owner",
            "2026-01-01",
            "",
        )
        .unwrap();
    }

    #[test]
    fn source_scopes_trusted_user_lookup() {
        let conn = opencrab_db::init_memory().unwrap();
        register(
            &conn,
            TRUSTED_PLATFORM_WEB,
            "42",
            TrustedUserPermission::User,
        );
        assert_eq!(
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_WEB, "42", "agent-1"),
            CallerIdentity::TrustedUser
        );
        assert_eq!(
            resolve_caller_identity(&conn, TRUSTED_PLATFORM_REST, "42", "agent-1"),
            CallerIdentity::Agent
        );
    }

    #[test]
    fn authenticated_owner_identifier_has_priority() {
        let conn = opencrab_db::init_memory().unwrap();
        assert_eq!(
            resolve_caller_identity_with_owner(
                &conn,
                "opaque-source",
                &["authenticated-id"],
                "agent-1",
                "authenticated-id",
            ),
            CallerIdentity::Owner
        );
    }

    #[test]
    fn empty_owner_never_matches() {
        let conn = opencrab_db::init_memory().unwrap();
        assert_eq!(
            resolve_caller_identity_with_owner(
                &conn,
                "opaque-source",
                &["anything"],
                "agent-1",
                "",
            ),
            CallerIdentity::Agent
        );
    }

    #[test]
    fn rest_co_agent_row_is_downgraded() {
        let conn = opencrab_db::init_memory().unwrap();
        register(
            &conn,
            TRUSTED_PLATFORM_REST,
            "self-asserted",
            TrustedUserPermission::CoAgent,
        );
        assert_eq!(
            resolve_rest_caller_identity(&conn, "self-asserted", "agent-1"),
            CallerIdentity::TrustedUser
        );
    }
}
