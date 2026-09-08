use super::*;

pub(super) const AGENT_ID: &str = "baseline-agent";
pub(super) const SESSION_ID: &str = "baseline-session";
pub(super) const TOOL_SESSION_ID: &str = "nostr-baseline-agent";
static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const COLLECTOR_WORKSPACE_TOKEN: &str = "{collector_workspace}";
const FIXTURE_EXECUTABLE_NAME: &str = "baseline-command";

pub(super) fn fixture_workspace(kind: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "opencrab-baseline-l2-{kind}-{}-{}",
        std::process::id(),
        FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

pub(super) fn capture_profile() -> Result<Value, String> {
    let missing_features = [
        (!cfg!(feature = "discord")).then_some("discord"),
        (!cfg!(feature = "nostr")).then_some("nostr"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if !missing_features.is_empty() {
        return Err(format!(
            "baseline full-production-surface-v1 requires Cargo features: {}",
            missing_features.join(", ")
        ));
    }
    if !cfg!(unix) {
        return Err(
            "baseline full-production-surface-v1 requires a Unix target with /bin/sh".to_string(),
        );
    }
    Ok(json!({
        "id": "full-production-surface-v1",
        "build": {
            "required_cargo_features": ["discord", "nostr"],
            "selection": "the baseline-l2 Cargo feature enables baseline-l1 and its exact feature set; ambient feature unification is not used",
            "target_family": "unix"
        },
        "runtime": {
            "database": "fresh in-memory database seeded by the collector for every probe",
            "configuration": "collector-owned AppState, tool, provider, MCP, and gateway fixtures; no operator config file is read",
            "environment": "the shell fixture receives an empty allowlisted environment; provider diagnostic fixtures inherit the parent environment, but execute a fixed collector-owned binary whose output does not read it",
            "filesystem": "fresh collector-owned workspaces with fixed contents; generated roots are normalized to <workspace>",
            "external_processes": "only a collector-owned executable fixture with fixed bytes and /bin/sh interpreter; no PATH lookup, operator binary, network service, or live gateway"
        }
    }))
}

pub(super) fn seed_fixture_executable(root: &Path) -> Result<std::path::PathBuf, String> {
    fs::create_dir_all(root).map_err(|error| format!("create baseline workspace: {error}"))?;
    let executable = root.join(FIXTURE_EXECUTABLE_NAME);
    fs::write(
        &executable,
        b"#!/bin/sh\nif [ \"${1-}\" = \"--version\" ]; then\n  printf 'baseline-cli 1.0\\n'\nelse\n  printf '%s' \"${1-}\"\nfi\n",
    )
    .map_err(|error| format!("seed baseline executable: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&executable)
            .map_err(|error| format!("read baseline executable metadata: {error}"))?
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions)
            .map_err(|error| format!("make baseline executable runnable: {error}"))?;
    }
    Ok(executable)
}

fn percent_encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

pub(super) fn materialize_uri(uri: &str) -> Result<String, String> {
    if !uri.contains(COLLECTOR_WORKSPACE_TOKEN) {
        return Ok(uri.to_string());
    }
    let import_root = fixture_workspace("import");
    fs::create_dir_all(import_root.join("import-source"))
        .map_err(|error| format!("seed baseline import source: {error}"))?;
    Ok(uri.replace(
        COLLECTOR_WORKSPACE_TOKEN,
        &percent_encode_query_value(&import_root.to_string_lossy()),
    ))
}

pub(super) fn captured_uri(uri: &str) -> String {
    uri.replace(COLLECTOR_WORKSPACE_TOKEN, "<workspace>")
}
