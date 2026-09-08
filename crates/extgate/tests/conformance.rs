//! V3 §9 conformance。mock gateway が omoikane 役。

include!("conformance/support.rs");
include!("conformance/liveness.rs");
include!("conformance/wire_protocol.rs");
include!("conformance/admin_registry.rs");
include!("conformance/said_ingress.rs");
include!("conformance/delivery.rs");
include!("conformance/turn_outcomes.rs");
include!("conformance/session_binding.rs");
include!("conformance/delivery_modes.rs");
include!("conformance/nostr_watch.rs");
include!("conformance/turn_queue.rs");
include!("conformance/operations.rs");
include!("conformance/hello_diagnostics.rs");
