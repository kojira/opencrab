//! Issue #1006 S3 static boundary assertions.
//! These scan production sources directly so obsolete shared routing shapes cannot silently return.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn source(path: &str) -> String {
    std::fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[test]
fn s3_shared_timed_fire_envelope_has_only_generic_binding_and_session_ids() {
    let timed_fire = source("crates/actions/src/timed_fire.rs");
    for forbidden in [
        "pub channel_id:",
        "pub guild_id:",
        "trait TransportFire",
        "struct TransportFireEnv",
        "register_descriptor",
    ] {
        assert!(
            !timed_fire.contains(forbidden),
            "platform-shaped/static timed-fire seam remains: {forbidden}"
        );
    }
    for required in ["pub binding_id:", "pub session_id:"] {
        assert!(
            timed_fire.contains(required),
            "generic timed-fire field is missing: {required}"
        );
    }
}

#[test]
fn s3_gateway_operation_projection_has_no_name_classification_fallback() {
    let projection = source("crates/extgate/src/ops_projection.rs");
    let gateway_traits = source("crates/gateway/src/traits.rs");
    assert!(
        !projection.contains("is_known_utterance_op"),
        "projection still classifies a gateway operation by name"
    );
    assert!(
        !gateway_traits.contains("pub fn is_known_utterance_op"),
        "shared runtime still exposes a static gateway-operation allowlist"
    );
}

#[test]
fn s3_shared_and_server_production_have_no_concrete_lifecycle_registry() {
    let actions = source("crates/actions/src/lib.rs");
    let server = source("crates/server/src/lib.rs");
    for forbidden in ["AgentGatewayLifecycle", "AgentGatewayRegistry"] {
        assert!(
            !actions.contains(forbidden) && !server.contains(forbidden),
            "concrete lifecycle registry remains reachable: {forbidden}"
        );
    }
}
