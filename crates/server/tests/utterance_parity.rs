//! Issue #1006 S3: utterance classification is declaration metadata, never a shared name list.
use serde_json::Value;

fn assert_metadata(declarations: Value) {
    for declaration in declarations.as_array().unwrap() {
        let dispatch = declaration["dispatch"].as_str().unwrap();
        let effect = declaration["effect"].as_str().unwrap();
        assert_eq!(dispatch == "utterance", effect == "utterance");
    }
}

#[test]
fn concrete_gateway_declarations_have_metadata_parity() {
    assert_metadata(opencrab_discord_gateway::ops::operation_declarations());
    assert_metadata(opencrab_nostr_gateway::ops::operation_declarations());
}

#[test]
fn concrete_gateway_declarations_pass_core_hello_validation() {
    for declarations in [
        opencrab_discord_gateway::ops::operation_declarations(),
        opencrab_nostr_gateway::ops::operation_declarations(),
    ] {
        let parsed = opencrab_extgate::validate_operations(&declarations, &|_| false).unwrap();
        opencrab_extgate::validate_runtime_compatibility(
            &parsed,
            opencrab_extgate::FinalDelivery::OperationDriven,
        )
        .unwrap();
    }
}
