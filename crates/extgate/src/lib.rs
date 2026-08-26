//! External gate V3 最小形。

pub mod admin;
pub mod bearer;
pub mod close;
pub mod delivery;
pub mod error;
pub mod ids;
pub mod inbound;
pub mod json;
pub mod listen;
pub mod protocol;
pub mod registry;

pub use admin::admin_router;
pub use inbound::channel_whitelisted;
pub use bearer::OperatorToken;
pub use error::{ErrorCode, GateError, UNAUTHORIZED_BODY};
pub use ids::{now_nanos, session_id_for_binding};
pub use listen::{recover_stale_deliveries, serve_uds, validate_listen_socket};
pub use registry::{ExtgateState, Registry};

use opencrab_actions::CallerIdentity;
use rusqlite::Connection;

/// server が `resolve_caller_identity_with_owner` を渡す。
pub type ResolveCallerFn = fn(&Connection, &str, &[&str], &str, &str) -> CallerIdentity;

/// query failure は Agent。owner 一致 → Owner、trusted → TrustedUser。
pub fn resolve_caller_fail_closed(
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
    if user_ids
        .iter()
        .any(|uid| opencrab_db::queries::is_trusted_user(conn, platform, uid, agent_id))
    {
        return CallerIdentity::TrustedUser;
    }
    CallerIdentity::Agent
}
