//! Startup must reject an explicitly invalid LLM provider instead of falling
//! back to the echo engine.

use std::process::Command;

fn assert_provider_stops_startup(value: &str, expected_error: &str) {
    let scratch = tempfile::tempdir().expect("create test directory");
    // The missing parent makes Unix bind fail immediately if provider
    // selection incorrectly continues, so this test can never hang.
    let socket = scratch.path().join("missing/core.sock");
    let output = Command::new(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .arg(socket)
        .arg(":memory:")
        .arg("room:test")
        .env("OPENCRAB_LLM_PROVIDER", value)
        .env_remove("OPENCRAB_PLACES")
        .output()
        .expect("run opencrab-social-runtime");

    assert!(
        !output.status.success(),
        "invalid provider must stop startup"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_error),
        "startup did not fail for the configured provider; stderr: {stderr}"
    );
}

#[test]
fn empty_provider_stops_startup_instead_of_using_echo() {
    assert_provider_stops_startup("", "unknown OPENCRAB_LLM_PROVIDER: ");
}

#[test]
fn unknown_provider_stops_startup_instead_of_using_echo() {
    assert_provider_stops_startup(
        "provider-that-does-not-exist",
        "unknown OPENCRAB_LLM_PROVIDER: provider-that-does-not-exist",
    );
}
