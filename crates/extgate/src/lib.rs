//! External gate V3 最小形。

pub mod admin;
pub mod bearer;
pub mod bundle;
pub mod close;
pub mod delivery;
pub mod delivery_mode;
pub mod error;
pub mod ids;
pub mod inbound;
pub mod json;
pub mod listen;
pub mod protocol;
pub mod race;
pub mod registry;
pub mod turn_queue;

pub use admin::admin_router;
pub use bearer::OperatorToken;
pub use bundle::NostrBundleAdmit;
pub use delivery_mode::{
    adjust_inbound_effect, delivery_mode_from_config_bytes, dispatches_v3_say, DeliveryMode,
};
pub use error::{ErrorCode, GateError, UNAUTHORIZED_BODY};
pub use ids::{config_digest, encode_config_b64, now_nanos, session_id_for_binding};
pub use inbound::channel_whitelisted;
pub use listen::{
    enqueue_bind, recover_stale_deliveries, serve_uds, validate_listen_socket, wait_bind_ack,
    web_binding_state, EnqueueBindOutcome,
};
pub use registry::{
    ExtgateState, NostrHeldTurn, NostrRelayFn, NostrSaidAdmit, NostrSaidDecision, NostrWatchSets,
    NostrWatchSetsFn, NostrWorkspaceFn, Registry,
};

use opencrab_actions::CallerIdentity;
use opencrab_db::queries::{
    get_trusted_user, is_trusted_co_agent, resolve_agent_by_discord_bot_user_id,
    resolve_agent_by_nostr_self_pubkey, TrustedUserPermission, TRUSTED_PLATFORM_DISCORD,
    TRUSTED_PLATFORM_NOSTR,
};
use rusqlite::Connection;

/// server が `resolve_caller_identity_with_owner` を渡す。
pub type ResolveCallerFn = fn(&Connection, &str, &[&str], &str, &str) -> CallerIdentity;

/// 本番と同じ 1 実装。owner → co_agent → trusted_user → Agent。
pub fn resolve_caller_identity_with_owner(
    conn: &Connection,
    platform: &str,
    user_ids: &[&str],
    agent_id: &str,
    owner_id: &str,
) -> CallerIdentity {
    if user_ids
        .iter()
        .any(|uid| opencrab_core::owner::is_owner_id(owner_id, uid))
    {
        return CallerIdentity::Owner;
    }
    if let Some(co_uuid) = resolve_co_agent_uuid(
        conn,
        platform,
        user_ids.first().copied().unwrap_or_default(),
    ) {
        if is_trusted_co_agent(conn, agent_id, &co_uuid).unwrap_or(false) {
            return CallerIdentity::CoAgent { agent_id: co_uuid };
        }
    }
    let permission = user_ids
        .iter()
        .find_map(|uid| get_trusted_user(conn, platform, uid, agent_id).map(|u| u.permission));
    match permission {
        Some(TrustedUserPermission::CoAgent) => CallerIdentity::CoAgent {
            agent_id: user_ids.first().copied().unwrap_or_default().to_string(),
        },
        Some(TrustedUserPermission::Owner) | Some(TrustedUserPermission::User) => {
            CallerIdentity::TrustedUser
        }
        None => CallerIdentity::Agent,
    }
}

fn resolve_co_agent_uuid(conn: &Connection, platform: &str, identifier: &str) -> Option<String> {
    match platform {
        TRUSTED_PLATFORM_DISCORD => resolve_agent_by_discord_bot_user_id(conn, identifier),
        TRUSTED_PLATFORM_NOSTR => resolve_agent_by_nostr_self_pubkey(conn, identifier),
        _ => None,
    }
}
