//! External gate V3 最小形。

pub mod admin;
pub mod bearer;
pub mod close;
pub mod completion;
pub mod delivery;
pub mod delivery_mode;
pub mod error;
pub mod fire;
pub mod ids;
pub mod inbound;
pub mod json;
pub mod listen;
pub mod operation_calls;
pub mod operations;
pub mod ops_projection;
pub mod protocol;
pub mod race;
pub mod registry;
pub mod turn_queue;

pub use admin::admin_router;
pub use bearer::OperatorToken;
pub use delivery_mode::{
    adjust_inbound_effect, delivery_mode_from_config_bytes, dispatches_v3_say, DeliveryMode,
};
pub use error::{ErrorCode, GateError, UNAUTHORIZED_BODY};
pub use fire::{ExtgateFire, ExtgateTimedFireSink, EXTGATE_TIMED_FIRE_KIND};
pub use ids::{config_digest, encode_config_b64, now_nanos, session_id_for_binding};
pub use inbound::channel_whitelisted;
pub use listen::{
    enqueue_bind, recover_stale_deliveries, serve_uds, validate_listen_socket, wait_bind_ack,
    web_binding_state, EnqueueBindOutcome,
};
pub use operation_calls::{invoke_and_wait, invoke_utterance, recover_stale_calls, InvokeError};
pub use operations::{
    declaration_digest, validate_operations, GatewayOperationDeclaration, OperationClass, Sharing,
    SubEngine,
};
pub use ops_projection::ExtgateOpsGatewayActions;
pub use registry::{ExtgateState, OperationOutcome, Registry, ReservedToolNameFn};
