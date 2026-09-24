use std::path::PathBuf;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn production_features_cannot_reach_plaintext_legacy_gate_admin_authorizer() {
    let root = crate_root();
    let cargo = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
    let registry = std::fs::read_to_string(root.join("src/registry.rs")).unwrap();

    assert!(
        cargo.contains("default = []"),
        "the default production build must not enable test/QC probes"
    );
    assert!(
        !registry.contains("legacy_admin_token")
            && !registry.contains("ExtgateState::new(db: Db, token: OperatorToken)"),
        "ExtgateState must have no production-feature plaintext authorizer state or constructor"
    );
    assert!(
        !lib.contains("pub use bearer::OperatorToken"),
        "the production crate API must not export OperatorToken"
    );
}
