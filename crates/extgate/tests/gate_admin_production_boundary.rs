use std::path::PathBuf;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn production_features_cannot_reach_plaintext_legacy_gate_admin_authorizer() {
    let root = crate_root();
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
    let registry = std::fs::read_to_string(root.join("src/registry.rs")).unwrap();

    assert!(
        !registry.contains("legacy_admin_token")
            && !registry.contains("ExtgateState::new(db: Db, token: OperatorToken)"),
        "ExtgateState must have no production-feature plaintext authorizer state or constructor"
    );
    assert!(
        !lib.contains("mod bearer") && !lib.contains("OperatorToken"),
        "no production feature may compile or export OperatorToken"
    );
    assert!(
        !root.join("src/bearer.rs").exists(),
        "the plaintext legacy authorizer must not remain in the production source tree"
    );
}
