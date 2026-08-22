//! Scenario-selected L2 baseline collector.
//!
//! This is feature-gated tooling, not production server behavior.  The
//! collector deliberately executes the current router/tool implementations and
//! serializes what they did; the checked-in scenario catalog contains inputs
//! and selection rationale, never response expectations.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use opencrab_core::engine::ActionExecutor;
use opencrab_gateway::GatewayActions;
use serde::Deserialize;
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::{create_router, process, test_app_state, AppState};

const AGENT_ID: &str = "baseline-agent";
const SESSION_ID: &str = "baseline-session";
const TOOL_SESSION_ID: &str = "web-baseline-agent-conversation";
static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const COLLECTOR_WORKSPACE_TOKEN: &str = "{collector_workspace}";
const FIXTURE_EXECUTABLE_NAME: &str = "baseline-command";

fn fixture_workspace(kind: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "opencrab-baseline-l2-{kind}-{}-{}",
        std::process::id(),
        FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn capture_profile() -> Result<Value, String> {
    let missing_features = [
        (!cfg!(feature = "discord")).then_some("discord"),
        (!cfg!(feature = "nostr")).then_some("nostr"),
        (!cfg!(feature = "web")).then_some("web"),
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
            "required_cargo_features": ["discord", "nostr", "web"],
            "selection": "the baseline-l2 Cargo feature enables baseline-l1 and its exact feature set; ambient feature unification is not used",
            "target_family": "unix"
        },
        "runtime": {
            "database": "fresh in-memory database seeded by the collector for every probe",
            "configuration": "collector-owned AppState, tool, provider, MCP, and gateway fixtures; no operator config file is read",
            "environment": "no parent environment value is a semantic input; fixture subprocesses receive an empty environment",
            "filesystem": "fresh collector-owned workspaces with fixed contents; generated roots are normalized to <workspace>",
            "external_processes": "only a collector-owned executable fixture with fixed bytes and /bin/sh interpreter; no PATH lookup, operator binary, network service, or live gateway"
        }
    }))
}

fn seed_fixture_executable(root: &Path) -> Result<std::path::PathBuf, String> {
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

fn materialize_uri(uri: &str) -> Result<String, String> {
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

fn captured_uri(uri: &str) -> String {
    uri.replace(COLLECTOR_WORKSPACE_TOKEN, "<workspace>")
}

#[derive(Clone, Debug, Deserialize)]
struct ScenarioCatalog {
    schema_version: u32,
    http: HttpScenarioCatalog,
    tool_visibility: ToolVisibilityCatalog,
    tool_execution: ToolScenarioCatalog,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolVisibilityCatalog {
    cases: Vec<ToolVisibilityScenario>,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolVisibilityScenario {
    name: String,
    caller: String,
    depth: u32,
    shell_enabled: bool,
    #[serde(default)]
    allowlist: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize)]
struct HttpScenarioCatalog {
    path_parameters: BTreeMap<String, PathParameter>,
    #[serde(default)]
    path_overrides: BTreeMap<String, PathParameter>,
    #[serde(default)]
    query_suffixes: BTreeMap<String, String>,
    normal_bodies: BTreeMap<String, Value>,
    normal_uncollected_l3: BTreeMap<String, String>,
    bodyless_alternates: BTreeMap<String, AlternateScenario>,
    #[serde(default)]
    mutation_postconditions: BTreeMap<String, HttpPostcondition>,
}

#[derive(Clone, Debug, Deserialize)]
struct PathParameter {
    normal: String,
    missing: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AlternateScenario {
    MissingResource,
    NotApplicable { reason: String },
    Uncollected { reason: String },
}

#[derive(Clone, Debug, Deserialize)]
struct HttpPostcondition {
    method: String,
    path: String,
    #[serde(default)]
    body: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolScenarioCatalog {
    success_arguments: BTreeMap<String, Value>,
    success_uncollected_l3: BTreeMap<String, String>,
    #[serde(default)]
    fixtures: BTreeMap<String, String>,
    #[serde(default)]
    postconditions: BTreeMap<String, ToolPostcondition>,
    #[serde(default)]
    effectful_tools: BTreeSet<String>,
    forwarding: BTreeMap<String, ForwardingScenario>,
}

#[derive(Clone, Debug, Deserialize)]
struct ForwardingScenario {
    tool: String,
    arguments: Value,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolPostcondition {
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    db_query: Option<String>,
    arguments: Value,
    #[serde(default)]
    expect_success: bool,
    #[serde(default)]
    expect_status: Option<u16>,
}

fn read_json(path: &Path) -> Result<Value, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn read_scenarios(path: &Path) -> Result<(Value, ScenarioCatalog), String> {
    let raw = read_json(path)?;
    let typed = serde_json::from_value(raw.clone())
        .map_err(|error| format!("parse typed scenario catalog {}: {error}", path.display()))?;
    Ok((raw, typed))
}

fn normalize(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(normalize),
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                normalize(value);
                if (key.ends_with("_at")
                    || (matches!(key.as_str(), "date_from" | "date_to")
                        && value.as_str().is_some_and(|s| s.contains('T'))))
                    && value.is_string()
                {
                    *value = Value::String("<timestamp>".to_string());
                }
                if let Some(text) = value.as_str() {
                    let generated = [("unit_id", "unit-"), ("core_id", "core-")]
                        .iter()
                        .find_map(|(field, prefix)| {
                            (key == *field).then(|| text.strip_prefix(prefix)).flatten()
                        })
                        .is_some_and(|suffix| {
                            suffix.len() == 24 && suffix.chars().all(|c| c.is_ascii_hexdigit())
                        });
                    if generated {
                        *value = Value::String(format!("<generated-{key}>"));
                    }
                }
                if matches!(key.as_str(), "duration_ms" | "latency_ms") && value.is_number() {
                    *value = Value::String("<duration>".to_string());
                }
                if key == "score" {
                    if let Some(number) = value.as_f64().and_then(|number| {
                        serde_json::Number::from_f64(
                            (number * 1_000_000_000_000.0).round() / 1_000_000_000_000.0,
                        )
                    }) {
                        *value = Value::Number(number);
                    }
                }
            }
        }
        Value::String(text) => normalize_string(text),
        _ => {}
    }
}

fn normalize_string(text: &mut String) {
    for (prefix, marker) in [
        ("unit-", "<generated-unit-id>"),
        ("core-", "<generated-core-id>"),
    ] {
        if text.strip_prefix(prefix).is_some_and(|suffix| {
            suffix.len() == 24
                && suffix
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
        }) {
            *text = marker.to_string();
            return;
        }
    }
    if uuid::Uuid::parse_str(text).is_ok() {
        *text = "<uuid>".to_string();
        return;
    }
    if text
        .strip_prefix("subtask-")
        .is_some_and(|suffix| uuid::Uuid::parse_str(suffix).is_ok())
    {
        *text = "subtask-<uuid>".to_string();
        return;
    }
    for marker in [
        "opencrab-baseline-l2-tools-",
        "opencrab-baseline-l2-workspace-",
        "opencrab-baseline-l2-process-",
        "opencrab-baseline-l2-import-",
    ] {
        let Some(marker_start) = text.find(marker) else {
            continue;
        };
        let path_start = text[..marker_start]
            .rfind(": ")
            .map(|start| start + 2)
            .or_else(|| text[..marker_start].rfind('=').map(|start| start + 1))
            .unwrap_or(0);
        let marker_end = marker_start + marker.len();
        let path_end = text[marker_end..]
            .find('/')
            .map_or(text.len(), |offset| marker_end + offset + 1);
        text.replace_range(path_start..path_end, "<workspace>/");
    }
}

fn seeded_state() -> Result<AppState, String> {
    let mut state = test_app_state();
    let workspace = fixture_workspace("workspace");
    fs::create_dir_all(&workspace)
        .map_err(|error| format!("create baseline workspace: {error}"))?;
    fs::write(workspace.join("baseline.txt"), b"baseline\n")
        .map_err(|error| format!("seed baseline workspace: {error}"))?;
    let executable = seed_fixture_executable(&fixture_workspace("process"))?;
    let mut llm_config: crate::config::LlmConfig =
        toml::from_str("[providers.codex]\n[providers.cursor]\n")
            .map_err(|error| format!("build deterministic diagnostic config: {error}"))?;
    for provider in ["codex", "cursor"] {
        llm_config
            .providers
            .get_mut(provider)
            .ok_or_else(|| format!("deterministic diagnostic config omitted {provider}"))?
            .binary_path = executable.to_string_lossy().to_string();
    }
    state.llm_config = Arc::new(llm_config);
    state.workspace_base = workspace.to_string_lossy().to_string();
    state.intake = Arc::new(crate::config::IntakeConfig {
        secrets: [("baseline-source".to_string(), "baseline-secret".to_string())]
            .into_iter()
            .collect(),
        routes: vec![crate::config::IntakeRoute {
            source: "baseline-source".to_string(),
            event_type: "baseline.event".to_string(),
            agent_id: AGENT_ID.to_string(),
        }],
        ..Default::default()
    });

    let conn = state.db.lock().map_err(|e| format!("lock DB: {e}"))?;
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: AGENT_ID.to_string(),
            name: "Baseline Agent".to_string(),
            job_title: Some("Compatibility Probe".to_string()),
            organization: Some("opencrab".to_string()),
            image_url: None,
            persona_name: "Baseline".to_string(),
            personality: Some("deterministic".to_string()),
            instructions: "baseline instructions".to_string(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .map_err(|e| format!("seed agent: {e}"))?;
    opencrab_db::queries::insert_session(
        &conn,
        &opencrab_db::queries::SessionRow {
            id: SESSION_ID.to_string(),
            mode: "baseline".to_string(),
            theme: "Compatibility".to_string(),
            phase: "active".to_string(),
            turn_number: 0,
            status: "active".to_string(),
            participant_ids_json: format!(r#"["{AGENT_ID}"]"#),
            facilitator_id: None,
            done_count: 0,
            max_turns: Some(1),
            metadata_json: None,
        },
    )
    .map_err(|e| format!("seed session: {e}"))?;
    opencrab_db::queries::insert_skill(
        &conn,
        &opencrab_db::queries::SkillRow {
            id: "baseline-skill".to_string(),
            agent_id: AGENT_ID.to_string(),
            name: "Seed Skill".to_string(),
            description: "seed".to_string(),
            situation_pattern: "seed".to_string(),
            guidance: "seed".to_string(),
            source_type: "baseline".to_string(),
            source_context: None,
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
            permission: "private".to_string(),
            archived: false,
            created_caller: Some("owner".to_string()),
            agent_visible: false,
        },
    )
    .map_err(|e| format!("seed skill: {e}"))?;
    opencrab_db::queries::insert_soul_preset(
        &conn,
        &opencrab_db::queries::SoulPresetRow {
            id: "baseline-preset".to_string(),
            agent_id: AGENT_ID.to_string(),
            preset_name: "Seed Preset".to_string(),
            persona_name: "Seed Persona".to_string(),
            custom_traits_json: Some("{}".to_string()),
        },
    )
    .map_err(|e| format!("seed soul preset: {e}"))?;
    opencrab_db::queries::upsert_curated_memory(
        &conn,
        &opencrab_db::queries::CuratedMemoryRow {
            id: "baseline-memory".to_string(),
            agent_id: AGENT_ID.to_string(),
            category: "baseline".to_string(),
            content: "seed memory".to_string(),
            created_at: "ignored by upsert".to_string(),
        },
    )
    .map_err(|e| format!("seed curated memory: {e}"))?;
    opencrab_db::queries::insert_trusted_co_agent(
        &conn,
        &opencrab_db::queries::TrustedCoAgentRow {
            id: "baseline-co-agent-row".to_string(),
            agent_id: AGENT_ID.to_string(),
            co_agent_id: "baseline-peer".to_string(),
            allowed_actions: None,
            created_by: "owner".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        },
    )
    .map_err(|e| format!("seed co-agent: {e}"))?;
    opencrab_db::queries::add_trusted_user(
        &conn,
        "web",
        "baseline-trusted-row",
        AGENT_ID,
        "baseline-user",
        opencrab_db::queries::TrustedUserPermission::User,
        "owner",
        "2026-01-01T00:00:00Z",
        "Baseline User",
    )
    .map_err(|e| format!("seed trusted user: {e}"))?;
    opencrab_db::queries::upsert_channel_config(
        &conn,
        &opencrab_db::queries::ChannelConfigRow {
            channel_id: "baseline-channel".to_string(),
            agent_id: AGENT_ID.to_string(),
            guild_id: "baseline-guild".to_string(),
            channel_name: "baseline".to_string(),
            readable: true,
            writable: true,
            whitelisted: true,
            heartbeat_enabled: false,
            heartbeat_interval_secs: None,
            heartbeat_instructions: String::new(),
        },
    )
    .map_err(|e| format!("seed channel config: {e}"))?;
    opencrab_db::queries::add_agent_allowed_command(
        &conn,
        AGENT_ID,
        "baseline-command",
        "baseline",
    )
    .map_err(|e| format!("seed allowed command: {e}"))?;
    opencrab_db::queries::upsert_agent_discord_config(
        &conn,
        &opencrab_db::queries::AgentDiscordConfigRow {
            agent_id: AGENT_ID.to_string(),
            bot_token: "baseline-not-a-credential".to_string(),
            owner_discord_id: "baseline-owner".to_string(),
            enabled: false,
        },
    )
    .map_err(|e| format!("seed Discord config: {e}"))?;
    opencrab_db::queries::upsert_agent_mcp_server(
        &conn,
        &opencrab_db::queries::AgentMcpServerRow {
            agent_id: AGENT_ID.to_string(),
            name: "baseline".to_string(),
            command: "false".to_string(),
            args_json: "[]".to_string(),
            env_json: "{}".to_string(),
            trusted_only: false,
            enabled: false,
        },
    )
    .map_err(|e| format!("seed MCP config: {e}"))?;
    opencrab_db::queries::upsert_agent_nostr_config(
        &conn,
        &opencrab_db::queries::AgentNostrConfigRow {
            agent_id: AGENT_ID.to_string(),
            secret_key: "nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqzqujme"
                .to_string(),
            relays_json: "[]".to_string(),
            filter_json: "{\"authors\":[],\"keywords\":[],\"kinds\":[]}".to_string(),
            enabled: false,
        },
    )
    .map_err(|e| format!("seed Nostr config: {e}"))?;
    opencrab_db::queries::insert_agent_schedule(
        &conn,
        &opencrab_db::queries::AgentScheduleRow {
            id: None,
            agent_id: AGENT_ID.to_string(),
            session_id: format!("nostr-{AGENT_ID}"),
            cron_expr: "0 0 * * *".to_string(),
            timezone: "Asia/Tokyo".to_string(),
            message: "seed schedule".to_string(),
            enabled: false,
            anchor_at: None,
            last_fired_at: None,
        },
    )
    .map_err(|e| format!("seed schedule: {e}"))?;
    drop(conn);
    Ok(state)
}

fn scenario_key(method: &str, path: &str) -> String {
    format!("{method} {path}")
}

fn concrete_uri(
    catalog: &HttpScenarioCatalog,
    template: &str,
    missing: bool,
) -> Result<String, String> {
    let mut path = if let Some(value) = catalog.path_overrides.get(template) {
        if missing {
            value.missing.clone()
        } else {
            value.normal.clone()
        }
    } else {
        template.to_string()
    };
    for (placeholder, value) in &catalog.path_parameters {
        path = path.replace(
            &format!("{{{placeholder}}}"),
            if missing {
                &value.missing
            } else {
                &value.normal
            },
        );
    }
    if path.contains('{') {
        return Err(format!(
            "scenario catalog has no value for path template {template}"
        ));
    }
    if !missing {
        if let Some(suffix) = catalog.query_suffixes.get(template) {
            path.push_str(suffix);
        }
    }
    Ok(path)
}

async fn request_once(
    state: AppState,
    name: &str,
    method: &str,
    uri: &str,
    body: Option<Vec<u8>>,
    content_type: Option<&str>,
) -> Result<Value, String> {
    let app = create_router(state);
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    if uri.starts_with("/api/hooks/baseline-source") {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let raw = body.as_deref().unwrap_or_default();
        let mut mac = Hmac::<Sha256>::new_from_slice(b"baseline-secret")
            .map_err(|e| format!("{name}: HMAC setup: {e}"))?;
        mac.update(raw);
        let signature = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        builder = builder.header("x-hook-signature", signature);
    }
    let request = builder
        .body(body.map(Body::from).unwrap_or_else(Body::empty))
        .map_err(|e| format!("{name}: build request: {e}"))?;
    let response = app
        .oneshot(request)
        .await
        .map_err(|e| format!("{name}: router call: {e}"))?;
    let status = response.status().as_u16();
    let mut headers: Vec<Value> = response
        .headers()
        .iter()
        .filter(|(name, _)| name.as_str() != "content-length")
        .map(|(name, value)| {
            value
                .to_str()
                .map(|v| json!({"name":name.as_str(),"value":v}))
                .map_err(|e| format!("{name}: invalid response header: {e}"))
        })
        .collect::<Result<_, _>>()?;
    headers.sort_by_key(|a| a.to_string());
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .map_err(|e| format!("{name}: read response: {e}"))?;
    let mut captured_body = serde_json::from_slice::<Value>(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    normalize(&mut captured_body);
    Ok(json!({
        "status": status,
        "headers": headers,
        "body": captured_body,
    }))
}

async fn collect_http(l1: &Value, catalog: &HttpScenarioCatalog) -> Result<Value, String> {
    let routes = l1
        .pointer("/http/routes")
        .and_then(Value::as_array)
        .ok_or_else(|| "L1 /http/routes is missing or not an array".to_string())?;
    let mut live_keys = BTreeSet::new();
    for route in routes {
        let path = route["path"]
            .as_str()
            .ok_or_else(|| "L1 route has no string path".to_string())?;
        for method in route["methods"]
            .as_array()
            .ok_or_else(|| format!("L1 route {path} has no methods"))?
        {
            live_keys.insert(scenario_key(
                method
                    .as_str()
                    .ok_or_else(|| format!("L1 route {path} has a non-string method"))?,
                path,
            ));
        }
    }
    for key in catalog
        .normal_bodies
        .keys()
        .chain(catalog.normal_uncollected_l3.keys())
        .chain(catalog.bodyless_alternates.keys())
        .chain(catalog.mutation_postconditions.keys())
    {
        if !live_keys.contains(key) {
            return Err(format!(
                "scenario catalog contains non-production route: {key}"
            ));
        }
    }

    let mut probes = Vec::new();
    let mut uncollected = Vec::new();
    for route in routes {
        let path = route["path"]
            .as_str()
            .ok_or_else(|| "L1 route has no string path".to_string())?;
        let methods = route["methods"]
            .as_array()
            .ok_or_else(|| format!("L1 route {path} has no methods"))?;
        for method in methods {
            let method = method
                .as_str()
                .ok_or_else(|| format!("L1 route {path} has a non-string method"))?;
            let key = scenario_key(method, path);
            let stem = format!(
                "{}__{}",
                method.to_ascii_lowercase(),
                path.replace('/', "_").replace(['{', '}', '*'], "")
            );

            if let Some(reason) = catalog.normal_uncollected_l3.get(&key) {
                uncollected.push(json!({
                    "name":format!("{stem}__normal"), "method":method, "path":path,
                    "branch":"normal", "status":"uncollected", "reason":reason, "level":"L3"
                }));
            } else {
                let catalog_uri = concrete_uri(catalog, path, false)?;
                let body_value = catalog.normal_bodies.get(&key).cloned();
                if matches!(method, "POST" | "PUT" | "PATCH")
                    && body_value.is_none()
                    && !catalog.bodyless_alternates.contains_key(&key)
                {
                    return Err(format!(
                        "mutating route has neither body nor explicit bodyless classification: {key}"
                    ));
                }
                let body = body_value
                    .as_ref()
                    .map(serde_json::to_vec)
                    .transpose()
                    .map_err(|error| format!("serialize {stem} normal body: {error}"))?;
                let state = seeded_state()?;
                let uri = materialize_uri(&catalog_uri)?;
                let artifact_uri = captured_uri(&catalog_uri);
                let observer = catalog.mutation_postconditions.get(&key);
                let before = if let Some(observer) = observer {
                    let observer_uri = concrete_uri(catalog, &observer.path, false)?;
                    let observer_body = observer
                        .body
                        .as_ref()
                        .map(serde_json::to_vec)
                        .transpose()
                        .map_err(|error| format!("serialize {stem} precondition body: {error}"))?;
                    Some(
                        request_once(
                            state.clone(),
                            &format!("{stem}__effect_before"),
                            &observer.method,
                            &observer_uri,
                            observer_body,
                            observer.body.as_ref().map(|_| "application/json"),
                        )
                        .await?,
                    )
                } else {
                    None
                };
                let response = request_once(
                    state.clone(),
                    &format!("{stem}__normal"),
                    method,
                    &uri,
                    body,
                    body_value.as_ref().map(|_| "application/json"),
                )
                .await?;
                let successful_mutation = matches!(method, "POST" | "PUT" | "PATCH" | "DELETE")
                    && response["status"]
                        .as_u64()
                        .is_some_and(|status| (200..300).contains(&status));
                let effect = if successful_mutation {
                    if let Some(observer) = observer {
                        let observer_uri = concrete_uri(catalog, &observer.path, false)?;
                        let observer_body = observer
                            .body
                            .as_ref()
                            .map(serde_json::to_vec)
                            .transpose()
                            .map_err(|error| {
                                format!("serialize {stem} postcondition body: {error}")
                            })?;
                        let after = request_once(
                            state,
                            &format!("{stem}__effect_after"),
                            &observer.method,
                            &observer_uri,
                            observer_body,
                            observer.body.as_ref().map(|_| "application/json"),
                        )
                        .await?;
                        if before.as_ref() == Some(&after) {
                            return Err(format!(
                                "HTTP postcondition for {key} did not change; a no-op handler would pass"
                            ));
                        }
                        json!({"status":"observed","request":{"method":observer.method,"uri":observer_uri,"body":observer.body},"before":before,"after":after})
                    } else {
                        json!({"status":"uncollected","reason":"scenario catalog declares no independent read-back for this successful mutation"})
                    }
                } else {
                    Value::Null
                };
                probes.push(json!({
                    "name":format!("{stem}__normal"),
                    "selection":"normal_or_local_precondition_branch",
                    "request":{"method":method,"uri":artifact_uri,"body":body_value},
                    "response":response,
                    "effect":effect
                }));
            }

            let (uri, body, content_type, selection) = if matches!(method, "POST" | "PUT" | "PATCH")
            {
                if let Some(alternate) = catalog.bodyless_alternates.get(&key) {
                    match alternate {
                        AlternateScenario::MissingResource => (
                            concrete_uri(catalog, path, true)?,
                            None,
                            None,
                            "missing_resource_rejection",
                        ),
                        AlternateScenario::NotApplicable { reason } => {
                            uncollected.push(json!({"name":format!("{stem}__reject_or_absent"),"method":method,"path":path,"branch":"input_rejection","status":"not_applicable","reason":reason}));
                            continue;
                        }
                        AlternateScenario::Uncollected { reason } => {
                            uncollected.push(json!({"name":format!("{stem}__reject_or_absent"),"method":method,"path":path,"branch":"input_rejection","status":"uncollected","reason":reason}));
                            continue;
                        }
                    }
                } else if catalog.normal_bodies.contains_key(&key) {
                    (
                        concrete_uri(catalog, path, false)?,
                        Some(b"{".to_vec()),
                        Some("application/json"),
                        "malformed_json_rejection",
                    )
                } else {
                    return Err(format!(
                        "bodyless route has no alternate classification: {key}"
                    ));
                }
            } else if method == "GET" && path == "/api/agents/{id}/web/stream" {
                uncollected.push(json!({"name":format!("{stem}__reject_or_absent"),"method":method,"path":path,"branch":"resource_absence_or_empty_state","status":"uncollected","reason":"the production SSE contract does not terminate","level":"L3"}));
                continue;
            } else {
                (
                    concrete_uri(catalog, path, true)?,
                    None,
                    None,
                    "resource_absence_or_empty_state",
                )
            };
            let response = request_once(
                seeded_state()?,
                &format!("{stem}__reject_or_absent"),
                method,
                &uri,
                body,
                content_type,
            )
            .await?;
            let status = response["status"].as_u64().unwrap_or_default();
            if matches!(
                selection,
                "malformed_json_rejection" | "missing_resource_rejection"
            ) && !(400..500).contains(&status)
            {
                return Err(format!(
                    "{key} {selection} did not reject: observed HTTP {status}"
                ));
            }
            probes.push(json!({
                "name":format!("{stem}__reject_or_absent"), "selection":selection,
                "request":{"method":method,"uri":uri,"body_utf8":if content_type.is_some(){Value::String("{".to_string())}else{Value::Null}},
                "response":response
            }));
        }
    }
    Ok(json!({"source_route_count":routes.len(),"probes":probes,"uncollected":uncollected}))
}

fn action_context(
    caller: opencrab_actions::CallerIdentity,
    fixture: &str,
) -> opencrab_actions::ActionContext {
    let root = fixture_workspace("tools");
    fs::create_dir_all(&root).expect("create baseline tool workspace");
    if fixture == "workspace_file" {
        fs::create_dir_all(root.join("captured")).expect("create captured fixture directory");
        fs::write(root.join("captured/file.txt"), b"baseline")
            .expect("write captured fixture file");
    }
    let workspace = opencrab_core::workspace::Workspace::from_root(root)
        .expect("baseline tool workspace must be valid");
    let db = opencrab_db::Db::memory().expect("in-memory baseline DB");
    {
        let conn = db.lock().expect("lock baseline tool DB");
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: AGENT_ID.to_string(),
                name: "Baseline Agent".to_string(),
                job_title: Some("Compatibility Probe".to_string()),
                organization: Some("opencrab".to_string()),
                image_url: None,
                persona_name: "Baseline".to_string(),
                personality: Some("deterministic".to_string()),
                instructions: "baseline instructions".to_string(),
                heartbeat_instructions: "baseline heartbeat".to_string(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .expect("seed baseline tool agent");
        opencrab_db::queries::insert_skill(
            &conn,
            &opencrab_db::queries::SkillRow {
                id: "baseline-seed-skill".to_string(),
                agent_id: AGENT_ID.to_string(),
                name: "Seed Skill".to_string(),
                description: "seed skill".to_string(),
                situation_pattern: "baseline".to_string(),
                guidance: "preserve behavior".to_string(),
                source_type: "baseline".to_string(),
                source_context: None,
                file_path: None,
                effectiveness: None,
                usage_count: 0,
                is_active: true,
                permission: "private".to_string(),
                archived: false,
                created_caller: Some("owner".to_string()),
                agent_visible: false,
            },
        )
        .expect("seed baseline tool skill");
        for content in ["baseline memory first", "baseline memory second"] {
            opencrab_db::queries::insert_session_log(
                &conn,
                &opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: AGENT_ID.to_string(),
                    session_id: SESSION_ID.to_string(),
                    log_type: "message".to_string(),
                    content: content.to_string(),
                    speaker_id: Some(AGENT_ID.to_string()),
                    turn_number: Some(1),
                    metadata_json: None,
                    created_at: Some("2026-01-01T00:00:00Z".to_string()),
                },
            )
            .expect("seed baseline tool history");
        }
        conn.execute(
            "UPDATE memory_sessions SET created_at = '2026-01-01T00:00:00Z' WHERE agent_id = ?1 AND session_id = ?2",
            rusqlite::params![AGENT_ID, SESSION_ID],
        )
        .expect("fix baseline history clock input");
        let node = |id: &str,
                    node_type: &str,
                    source_type: &str,
                    title: &str,
                    short_id: &str,
                    start: Option<i64>,
                    end: Option<i64>,
                    keywords_json: &str| opencrab_db::queries::IndexNodeRow {
            id: id.to_string(),
            agent_id: AGENT_ID.to_string(),
            parent_id: None,
            node_type: node_type.to_string(),
            source_type: source_type.to_string(),
            title: title.to_string(),
            summary: format!("{title} summary"),
            start_log_id: start,
            end_log_id: end,
            source_session_id: None,
            date_from: Some("2026-01-01".to_string()),
            date_to: Some("2026-01-01".to_string()),
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: Some(short_id.to_string()),
            keywords_json: keywords_json.to_string(),
            summary_refreshed_at: None,
        };
        for n in [
            node(
                "baseline-topic",
                "topic",
                "session_log",
                "Baseline Topic",
                "t1",
                Some(1),
                Some(2),
                "[]",
            ),
            node(
                "baseline-unit-source",
                "unit",
                "declared",
                "Baseline Unit Source",
                "u1",
                Some(1),
                Some(2),
                "[]",
            ),
            node(
                "baseline-unit-retract",
                "unit",
                "declared",
                "Baseline Unit Retract",
                "u2",
                Some(1),
                Some(1),
                "[]",
            ),
            node(
                "baseline-core-update",
                "meta",
                "condensed",
                "Baseline Core Update",
                "m1",
                Some(1),
                Some(2),
                "[\"u1\"]",
            ),
            node(
                "baseline-core-retract",
                "meta",
                "condensed",
                "Baseline Core Retract",
                "m2",
                Some(1),
                Some(2),
                "[\"u1\"]",
            ),
            node(
                "baseline-tag",
                "category",
                "category",
                "Baseline Tag",
                "c1",
                None,
                None,
                "[]",
            ),
            node(
                "baseline-merge-from",
                "category",
                "category",
                "Merge From",
                "c2",
                None,
                None,
                "[]",
            ),
            node(
                "baseline-merge-into",
                "category",
                "category",
                "Merge Into",
                "c3",
                None,
                None,
                "[]",
            ),
        ] {
            opencrab_db::queries::insert_index_node(&conn, &n)
                .expect("seed baseline tool memory node");
        }
        if fixture == "active_task" {
            opencrab_db::queries::insert_task_ledger(
                &conn,
                AGENT_ID,
                TOOL_SESSION_ID,
                "fixture task",
                Some("fixture contract"),
            )
            .expect("seed active task fixture");
        }
        if fixture == "active_schedule" {
            opencrab_db::queries::insert_agent_schedule(
                &conn,
                &opencrab_db::queries::AgentScheduleRow {
                    id: None,
                    agent_id: AGENT_ID.to_string(),
                    session_id: TOOL_SESSION_ID.to_string(),
                    cron_expr: "0 9 * * *".to_string(),
                    timezone: "UTC".to_string(),
                    message: "fixture schedule".to_string(),
                    enabled: true,
                    anchor_at: None,
                    last_fired_at: None,
                },
            )
            .expect("seed active schedule fixture");
        }
        if fixture == "llm_metrics" {
            opencrab_db::queries::insert_llm_metrics(
                &conn,
                &opencrab_db::queries::LlmMetricsRow {
                    id: "baseline-metrics".to_string(),
                    agent_id: AGENT_ID.to_string(),
                    session_id: Some(TOOL_SESSION_ID.to_string()),
                    timestamp: "2026-01-01T00:00:00Z".to_string(),
                    provider: "baseline".to_string(),
                    model: "baseline:model".to_string(),
                    purpose: "baseline".to_string(),
                    task_type: None,
                    complexity: None,
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    estimated_cost_usd: 0.0,
                    latency_ms: 1,
                    time_to_first_token_ms: None,
                },
            )
            .expect("seed LLM metrics fixture");
        }
        if fixture == "allowed_command" {
            opencrab_db::queries::add_agent_allowed_command(
                &conn,
                AGENT_ID,
                "baseline_cmd",
                "baseline",
            )
            .expect("seed allowed command fixture");
        }
        if fixture == "archived_skill" {
            opencrab_db::queries::archive_skill(&conn, "baseline-seed-skill", true)
                .expect("seed archived skill fixture");
        }
        if fixture == "tagged_topic" {
            opencrab_db::queries::assign_topic_to_category(
                &conn,
                AGENT_ID,
                "baseline-topic",
                "baseline-tag",
                "2026-01-01T00:00:00Z",
            )
            .expect("seed tagged topic fixture");
        }
    }
    opencrab_actions::ActionContext {
        agent_id: AGENT_ID.to_string(),
        agent_name: "Baseline Agent".to_string(),
        session_id: Some(TOOL_SESSION_ID.to_string()),
        db,
        workspace: Arc::new(workspace),
        last_metrics_id: Arc::new(std::sync::Mutex::new(
            (fixture == "llm_metrics").then(|| "baseline-metrics".to_string()),
        )),
        model_override: Arc::new(std::sync::Mutex::new(None)),
        current_purpose: Arc::new(std::sync::Mutex::new("baseline".to_string())),
        caller,
        runtime_info: Arc::new(std::sync::Mutex::new(opencrab_actions::RuntimeInfo {
            default_model: "baseline:model".to_string(),
            active_model: None,
            available_providers: vec!["baseline".to_string()],
            gateway: "baseline".to_string(),
        })),
    }
}

fn seeded_tool_state() -> AppState {
    let state = test_app_state();
    {
        let conn = state.db.lock().expect("lock baseline gateway DB");
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: AGENT_ID.to_string(),
                name: "Baseline Agent".to_string(),
                job_title: Some("Compatibility Probe".to_string()),
                organization: Some("opencrab".to_string()),
                image_url: None,
                persona_name: "Baseline".to_string(),
                personality: Some("deterministic".to_string()),
                instructions: "baseline instructions".to_string(),
                heartbeat_instructions: "baseline heartbeat".to_string(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .expect("seed baseline gateway agent");
        #[cfg(feature = "nostr")]
        opencrab_db::queries::upsert_agent_nostr_config(
            &conn,
            &opencrab_db::queries::AgentNostrConfigRow {
                agent_id: AGENT_ID.to_string(),
                enabled: false,
                relays_json: "[]".to_string(),
                filter_json: "{\"authors\":[],\"keywords\":[],\"kinds\":[]}".to_string(),
                secret_key: "nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqzqujme"
                    .to_string(),
            },
        )
        .expect("seed baseline Nostr config");
    }
    state
}

#[derive(Clone)]
struct LocalMcpServer {
    trusted_name: String,
    tools: Vec<opencrab_mcp::McpTool>,
}

#[async_trait]
impl opencrab_mcp::actions::McpServer for LocalMcpServer {
    fn server_name(&self) -> &str {
        &self.trusted_name
    }

    fn tools(&self) -> &[opencrab_mcp::McpTool] {
        &self.tools
    }

    async fn call_tool(
        &self,
        name: &str,
        args: Value,
    ) -> anyhow::Result<opencrab_mcp::McpToolResult> {
        match args.get("mode").and_then(Value::as_str) {
            Some("tool_error") => Ok(opencrab_mcp::McpToolResult {
                text: "local MCP tool error".to_string(),
                is_error: true,
            }),
            Some("transport_error") => anyhow::bail!("local MCP transport closed"),
            _ => Ok(opencrab_mcp::McpToolResult {
                text: format!(
                    "{name}:{}",
                    args.get("value").and_then(Value::as_str).unwrap_or("ok")
                ),
                is_error: false,
            }),
        }
    }
}

fn mcp_servers() -> Vec<opencrab_mcp::ConnectedServer> {
    let tool = |description: &str| opencrab_mcp::McpTool {
        name: "echo".to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type":"object",
            "properties":{"value":{"type":"string"},"mode":{"type":"string"}},
            "required":["value"]
        }),
    };
    vec![
        opencrab_mcp::ConnectedServer {
            server: Arc::new(LocalMcpServer {
                trusted_name: "public_local".to_string(),
                tools: vec![tool("public local echo")],
            }),
            trusted_only: false,
        },
        opencrab_mcp::ConnectedServer {
            server: Arc::new(LocalMcpServer {
                trusted_name: "trusted_local".to_string(),
                tools: vec![tool("trusted local echo")],
            }),
            trusted_only: true,
        },
    ]
}

fn tool_names(executor: &opencrab_actions::BridgedExecutor) -> Vec<String> {
    let mut names: Vec<_> = executor.list_tools().into_iter().map(|d| d.name).collect();
    names.sort();
    names
}

fn build_executor_with_state(
    caller: opencrab_actions::CallerIdentity,
    depth: u32,
    shell_enabled: bool,
    allowlist: Option<Vec<String>>,
    fixture: &str,
) -> (opencrab_actions::BridgedExecutor, AppState) {
    let context = action_context(caller, fixture);
    let mut state = seeded_tool_state();
    state.db = context.db.clone();
    state.workspace_base = context.workspace.root().to_string_lossy().to_string();
    let fixture_command = shell_enabled.then(|| {
        seed_fixture_executable(&fixture_workspace("process"))
            .expect("seed baseline process fixture")
            .to_string_lossy()
            .to_string()
    });
    #[cfg(feature = "nostr")]
    {
        let connection = state.db.lock().expect("lock baseline Nostr DB");
        opencrab_db::queries::upsert_agent_nostr_config(
            &connection,
            &opencrab_db::queries::AgentNostrConfigRow {
                agent_id: AGENT_ID.to_string(),
                enabled: false,
                relays_json: "[]".to_string(),
                filter_json: "{\"authors\":[],\"keywords\":[],\"kinds\":[]}".to_string(),
                secret_key: "nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqzqujme"
                    .to_string(),
            },
        )
        .expect("seed baseline Nostr config");
    }
    *state.tools_config.write().expect("baseline tools config") = opencrab_actions::ToolsConfig {
        enabled: shell_enabled,
        shell: shell_enabled.then(|| opencrab_actions::ShellToolConfig {
            allowed_commands: vec![fixture_command.expect("enabled shell fixture")],
            allowed_env_vars: Vec::new(),
            ..Default::default()
        }),
    };
    let executor = process::build_turn_executor(
        &state,
        process::TurnExecutorWiring {
            context,
            depth,
            gateway_actions: None,
            subtask_registry: Arc::new(dashmap::DashMap::new()),
            completion_sink: None,
            subtask_starts: None,
            reply_target: None,
            tool_allowlist: allowlist,
        },
        |caller_is_trusted| {
            Some(Arc::new(opencrab_mcp::McpToolProvider::new(
                mcp_servers(),
                caller_is_trusted,
            )) as Arc<dyn GatewayActions>)
        },
    );
    (executor, state)
}

fn build_executor(
    caller: opencrab_actions::CallerIdentity,
    depth: u32,
    shell_enabled: bool,
    allowlist: Option<Vec<String>>,
    fixture: &str,
) -> opencrab_actions::BridgedExecutor {
    build_executor_with_state(caller, depth, shell_enabled, allowlist, fixture).0
}

fn subtask_fixture_registry(
    subtask_id: &str,
    sub_session_id: &str,
    parent_session_id: &str,
    steerable: bool,
) -> opencrab_actions::SubtaskRegistry {
    let registry: opencrab_actions::SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    registry.insert(
        subtask_id.to_string(),
        opencrab_actions::SpawnedSubtask {
            abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            session_id: sub_session_id.to_string(),
            parent_session_id: parent_session_id.to_string(),
            agent_id: AGENT_ID.to_string(),
            label: "baseline subtask".to_string(),
            tool_name: "spawn_subtask".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: None,
            caller: opencrab_actions::CallerIdentity::Owner,
            lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            steerable,
        },
    );
    registry
}

fn build_subtask_fixture_executor(
    session_id: &str,
    depth: u32,
    registry: opencrab_actions::SubtaskRegistry,
) -> (opencrab_actions::BridgedExecutor, AppState) {
    let mut context = action_context(opencrab_actions::CallerIdentity::Owner, "default");
    context.session_id = Some(session_id.to_string());
    let mut state = seeded_tool_state();
    state.db = context.db.clone();
    state.workspace_base = context.workspace.root().to_string_lossy().to_string();
    let executor = process::build_turn_executor(
        &state,
        process::TurnExecutorWiring {
            context,
            depth,
            gateway_actions: None,
            subtask_registry: registry,
            completion_sink: None,
            subtask_starts: None,
            reply_target: None,
            tool_allowlist: None,
        },
        |_| None,
    );
    (executor, state)
}

fn collect_visibility(catalog: &ToolVisibilityCatalog) -> Result<Value, String> {
    use opencrab_actions::CallerIdentity;
    let mut names = BTreeSet::new();
    let rows = catalog
        .cases
        .iter()
        .map(|scenario| {
            if !names.insert(&scenario.name) {
                return Err(format!(
                    "duplicate tool visibility scenario: {}",
                    scenario.name
                ));
            }
            let caller = match scenario.caller.as_str() {
                "owner" => CallerIdentity::Owner,
                "co_agent" => CallerIdentity::CoAgent {
                    agent_id: "baseline-peer".to_string(),
                },
                "trusted_user" => CallerIdentity::TrustedUser,
                "agent" => CallerIdentity::Agent,
                other => return Err(format!("unknown visibility caller: {other}")),
            };
            let caller_name = match &caller {
                CallerIdentity::Owner => "owner",
                CallerIdentity::Agent => "agent",
                CallerIdentity::TrustedUser => "trusted_user",
                CallerIdentity::CoAgent { .. } => "co_agent",
            };
            let mcp_trusted = !matches!(caller, CallerIdentity::Agent);
            let executor = build_executor(
                caller,
                scenario.depth,
                scenario.shell_enabled,
                scenario.allowlist.clone(),
                "default",
            );
            Ok(json!({
                "name":scenario.name,
                "dimensions": {"caller":caller_name,"depth":scenario.depth,"shell_enabled":scenario.shell_enabled,"allowlist":scenario.allowlist,"mcp_caller_is_trusted":mcp_trusted},
                "visible_tools": tool_names(&executor)
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({"scenarios":rows}))
}

fn required_args(schema: &Value) -> Vec<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn result_json(result: opencrab_core::ActionResult) -> Value {
    let mut value = json!({"success":result.success,"data":result.data,"error":result.error});
    normalize(&mut value);
    value
}

fn session_log_count(state: &AppState, session_id: &str) -> Result<i64, String> {
    state
        .db
        .lock()
        .map_err(|error| format!("session log observer lock: {error}"))?
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
            rusqlite::params![session_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("session log observer query: {error}"))
}

async fn observe_tool_postcondition(
    executor: &opencrab_actions::BridgedExecutor,
    state: &AppState,
    scenario: &ToolPostcondition,
    name: &str,
) -> Result<Value, String> {
    match (
        &scenario.tool,
        &scenario.method,
        &scenario.uri,
        &scenario.db_query,
    ) {
        (Some(tool), None, None, None) => Ok(result_json(
            executor.execute(tool, &scenario.arguments).await,
        )),
        (None, Some(method), Some(uri), None) => {
            let body = (!scenario.arguments.is_null())
                .then(|| serde_json::to_vec(&scenario.arguments))
                .transpose()
                .map_err(|error| format!("serialize {name} postcondition body: {error}"))?;
            request_once(
                state.clone(),
                name,
                method,
                uri,
                body,
                (!scenario.arguments.is_null()).then_some("application/json"),
            )
            .await
        }
        (None, None, None, Some(query)) if query == "memory_declare_window" => {
            let connection = state
                .db
                .lock()
                .map_err(|error| format!("{name} DB observer lock: {error}"))?;
            let mut value = serde_json::to_value(
                opencrab_db::queries::get_memory_declare_window(&connection, AGENT_ID)
                    .map_err(|error| format!("{name} DB observer: {error}"))?,
            )
            .map_err(|error| format!("{name} DB observer serialize: {error}"))?;
            normalize(&mut value);
            Ok(json!({"db_query":query,"value":value}))
        }
        (None, None, None, Some(query)) if query == "executor_runtime" => {
            let runtime = executor.runtime_state();
            Ok(json!({
                "db_query":query,
                "value":{
                    "model_override":runtime.model_override,
                    "current_purpose":runtime.current_purpose,
                }
            }))
        }
        (None, None, None, Some(query)) if query == "webhook_configs" => {
            let connection = state
                .db
                .lock()
                .map_err(|error| format!("{name} DB observer lock: {error}"))?;
            let rows =
                opencrab_db::queries::list_agent_webhook_config(&connection, Some(AGENT_ID), true)
                    .map_err(|error| format!("{name} DB observer: {error}"))?;
            let mut value = serde_json::to_value(rows)
                .map_err(|error| format!("{name} DB observer serialize: {error}"))?;
            normalize(&mut value);
            Ok(json!({"db_query":query,"value":value}))
        }
        (None, None, None, Some(query)) if query == "memory_index_state" => {
            let connection = state
                .db
                .lock()
                .map_err(|error| format!("{name} DB observer lock: {error}"))?;
            let mut nodes_statement = connection
                .prepare("SELECT id, title, node_type, source_type, summary, keywords_json FROM memory_index_nodes WHERE agent_id = ?1 ORDER BY id")
                .map_err(|error| format!("{name} nodes observer prepare: {error}"))?;
            let nodes = nodes_statement
                .query_map(rusqlite::params![AGENT_ID], |row| {
                    Ok(json!({"id":row.get::<_,String>(0)?,"title":row.get::<_,String>(1)?,"node_type":row.get::<_,String>(2)?,"source_type":row.get::<_,String>(3)?,"summary":row.get::<_,String>(4)?,"keywords_json":row.get::<_,String>(5)?}))
                })
                .map_err(|error| format!("{name} nodes observer query: {error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("{name} nodes observer row: {error}"))?;
            let mut members_statement = connection
                .prepare("SELECT topic_id, category_id FROM memory_category_members WHERE agent_id = ?1 ORDER BY topic_id, category_id")
                .map_err(|error| format!("{name} members observer prepare: {error}"))?;
            let members = members_statement
                .query_map(rusqlite::params![AGENT_ID], |row| {
                    Ok(json!({"topic_id":row.get::<_,String>(0)?,"category_id":row.get::<_,String>(1)?}))
                })
                .map_err(|error| format!("{name} members observer query: {error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("{name} members observer row: {error}"))?;
            Ok(json!({"db_query":query,"nodes":nodes,"members":members}))
        }
        (None, None, None, Some(query)) if query == "memory_index_config" => {
            let connection = state
                .db
                .lock()
                .map_err(|error| format!("{name} DB observer lock: {error}"))?;
            let mut value = serde_json::to_value(
                opencrab_db::queries::get_memory_index_config(&connection, AGENT_ID)
                    .map_err(|error| format!("{name} DB observer: {error}"))?,
            )
            .map_err(|error| format!("{name} DB observer serialize: {error}"))?;
            normalize(&mut value);
            Ok(json!({"db_query":query,"value":value}))
        }
        _ => Err(format!(
            "postcondition for {name} must select exactly one tool or HTTP method+uri"
        )),
    }
}

async fn collect_tool_execution(catalog: &ToolScenarioCatalog) -> Result<Value, String> {
    use opencrab_actions::CallerIdentity;
    let owner = build_executor(CallerIdentity::Owner, 0, true, None, "default");
    let all_defs = owner.list_tools();
    let live_tools: BTreeSet<_> = all_defs
        .iter()
        .map(|definition| definition.name.clone())
        .collect();
    let selected_tools: BTreeSet<_> = catalog
        .success_arguments
        .keys()
        .chain(catalog.success_uncollected_l3.keys())
        .cloned()
        .collect();
    let overlap: Vec<_> = catalog
        .success_arguments
        .keys()
        .filter(|name| catalog.success_uncollected_l3.contains_key(*name))
        .cloned()
        .collect();
    if !overlap.is_empty() || live_tools != selected_tools {
        return Err(format!(
            "tool scenario catalog is not a bijection with production tools; overlap={overlap:?}, missing={:?}, unknown={:?}",
            live_tools.difference(&selected_tools).collect::<Vec<_>>(),
            selected_tools.difference(&live_tools).collect::<Vec<_>>()
        ));
    }
    let missing_postconditions: Vec<_> = catalog
        .effectful_tools
        .iter()
        .filter(|tool| !catalog.postconditions.contains_key(*tool))
        .collect();
    if !missing_postconditions.is_empty() {
        return Err(format!(
            "effectful tool scenarios lack postconditions: {missing_postconditions:?}"
        ));
    }
    let mut required_missing = Vec::new();
    let mut no_required_args_uncollected = Vec::new();
    for def in &all_defs {
        let required = required_args(&def.parameters);
        if required.is_empty() {
            no_required_args_uncollected.push(json!({
                "tool":def.name,
                "facet":"missing_arguments",
                "status":"not_applicable",
                "reason":"observed schema declares no required arguments; invoking {} would not be an argument-missing probe"
            }));
            continue;
        }
        let result = owner.execute(&def.name, &json!({})).await;
        required_missing.push(json!({
            "tool":def.name,
            "required_by_observed_schema":required,
            "arguments":{},
            "result":result_json(result)
        }));
    }

    let agent = build_executor(CallerIdentity::Agent, 0, true, None, "default");
    let mut permission = Vec::new();
    for def in &all_defs {
        let policy = opencrab_actions::tool_policy(&def.name);
        if policy.owner_only || policy.trusted_only || def.name == "mcp__trusted_local__echo" {
            let result = agent.execute(&def.name, &json!({})).await;
            permission.push(json!({
                "tool":def.name,
                "caller":"agent",
                "policy":{"owner_only":policy.owner_only,"trusted_only":policy.trusted_only,"mcp_trusted_only":def.name == "mcp__trusted_local__echo"},
                "result":result_json(result)
            }));
        }
    }

    let success_cases = &catalog.success_arguments;
    let mut successes = Vec::new();
    let mut unsuccessful_attempts = Vec::new();
    for (tool, arguments) in success_cases {
        if catalog
            .fixtures
            .get(tool)
            .is_some_and(|fixture| fixture.starts_with("subtask_"))
        {
            continue;
        }
        let fixture = catalog
            .fixtures
            .get(tool)
            .map(String::as_str)
            .unwrap_or("default");
        let (executor, state) =
            build_executor_with_state(CallerIdentity::Owner, 0, true, None, fixture);
        let before = if let Some(postcondition) = catalog.postconditions.get(tool) {
            Some(observe_tool_postcondition(&executor, &state, postcondition, tool).await?)
        } else {
            None
        };
        let mut execution_arguments = arguments.clone();
        if tool == "execute_shell" {
            let command = state
                .tools_config
                .read()
                .map_err(|error| format!("read baseline tools config: {error}"))?
                .shell
                .as_ref()
                .and_then(|shell| shell.allowed_commands.first())
                .cloned()
                .ok_or_else(|| "baseline shell fixture is missing".to_string())?;
            execution_arguments["command"] = command.into();
        }
        let result = executor.execute(tool, &execution_arguments).await;
        let result = result_json(result);
        normalize(&mut execution_arguments);
        if result["success"] == true {
            let postcondition = if let Some(postcondition) = catalog.postconditions.get(tool) {
                let observed =
                    observe_tool_postcondition(&executor, &state, postcondition, tool).await?;
                if postcondition.tool.is_some()
                    && observed["success"] != postcondition.expect_success
                {
                    return Err(format!(
                        "postcondition for {tool} did not satisfy expected success={}: {observed}",
                        postcondition.expect_success
                    ));
                }
                if let Some(expected) = postcondition.expect_status {
                    if observed["status"] != expected {
                        return Err(format!("postcondition for {tool} did not satisfy expected HTTP status={expected}: {observed}"));
                    }
                }
                if before.as_ref() == Some(&observed) {
                    return Err(format!(
                        "postcondition for {tool} did not change; a no-op implementation would pass"
                    ));
                }
                Some(json!({
                    "tool":postcondition.tool,
                    "method":postcondition.method,
                    "uri":postcondition.uri,
                    "db_query":postcondition.db_query,
                    "arguments":postcondition.arguments,
                    "expect_success":postcondition.expect_success,
                    "before":before,
                    "observed":observed
                }))
            } else {
                None
            };
            successes.push(json!({"tool":tool,"arguments":execution_arguments,"result":result,"postcondition":postcondition}));
        } else {
            unsuccessful_attempts.push(json!({
                "tool":tool,
                "arguments":execution_arguments,
                "result":result,
                "status":"uncollected",
                "reason":"the selected local success precondition reached the real implementation but did not return success"
            }));
        }
    }

    for (tool, fixture) in &catalog.fixtures {
        let Some((session_id, depth, steerable)) = (match fixture.as_str() {
            "subtask_spawn" => Some((TOOL_SESSION_ID, 0, false)),
            "subtask_cancel" => Some((TOOL_SESSION_ID, 0, false)),
            "subtask_steer" => Some((TOOL_SESSION_ID, 0, true)),
            "subtask_report" => Some(("subtask-baseline-subtask", 1, true)),
            _ => None,
        }) else {
            continue;
        };
        let arguments = catalog
            .success_arguments
            .get(tool)
            .ok_or_else(|| format!("fixture {fixture} has no success arguments for {tool}"))?;
        let registry = if fixture == "subtask_spawn" {
            Arc::new(dashmap::DashMap::new())
        } else {
            subtask_fixture_registry(
                "baseline-subtask",
                "subtask-baseline-subtask",
                TOOL_SESSION_ID,
                steerable,
            )
        };
        let registry_observer = registry.clone();
        let (executor, state) = build_subtask_fixture_executor(session_id, depth, registry);
        let before_logs = if matches!(fixture.as_str(), "subtask_steer" | "subtask_report") {
            let observed_session = if fixture == "subtask_steer" {
                "subtask-baseline-subtask"
            } else {
                TOOL_SESSION_ID
            };
            Some(session_log_count(&state, observed_session)?)
        } else {
            None
        };
        let raw_result = executor.execute(tool, arguments).await;
        let raw_data = raw_result.data.clone();
        let result = result_json(raw_result);
        let effect = match fixture.as_str() {
            "subtask_spawn" if result["success"] == true => {
                let subtask_id = raw_data["subtask_id"]
                    .as_str()
                    .ok_or_else(|| "spawn_subtask succeeded without a subtask_id".to_string())?;
                let session_id = raw_data["session_id"]
                    .as_str()
                    .ok_or_else(|| "spawn_subtask succeeded without a session_id".to_string())?;
                let durable_session_registered = state
                    .db
                    .lock()
                    .map_err(|error| format!("spawn_subtask DB observer lock: {error}"))?
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                        rusqlite::params![session_id],
                        |row| row.get::<_, bool>(0),
                    )
                    .map_err(|error| format!("spawn_subtask DB observer: {error}"))?;
                if !durable_session_registered {
                    return Err(
                        "spawn_subtask returned success without registering its durable session"
                            .to_string(),
                    );
                }
                Some(json!({
                    "accepted":true,
                    "registry_registered_when_observed":registry_observer.contains_key(subtask_id),
                    "durable_session_registered":true,
                    "completion":{
                        "status":"uncollected",
                        "reason":"the production spawn path was accepted and registered; its background LLM completion has no provider and is not claimed as success"
                    }
                }))
            }
            "subtask_cancel" if result["success"] == true => {
                if registry_observer.contains_key("baseline-subtask") {
                    return Err(
                        "cancel_subtask returned success without removing the registry entry"
                            .to_string(),
                    );
                }
                Some(json!({"registry_before":true,"registry_after":false}))
            }
            "subtask_steer" | "subtask_report" if result["success"] == true => {
                let observed_session = if fixture == "subtask_steer" {
                    "subtask-baseline-subtask"
                } else {
                    TOOL_SESSION_ID
                };
                let after_logs = session_log_count(&state, observed_session)?;
                if before_logs == Some(after_logs) {
                    return Err(format!(
                        "{tool} returned success without recording its delivered effect"
                    ));
                }
                Some(
                    json!({"session_id":observed_session,"logs_before":before_logs,"logs_after":after_logs}),
                )
            }
            _ => None,
        };
        if result["success"] == true {
            successes
                .push(json!({"tool":tool,"arguments":arguments,"result":result,"effect":effect}));
        } else {
            unsuccessful_attempts.push(json!({
                "tool":tool,
                "arguments":arguments,
                "result":result,
                "status":"uncollected",
                "reason":"the selected in-memory running-subtask precondition reached the real implementation but did not return success"
            }));
        }
    }

    let mut forwarding = Vec::new();
    for (name, scenario) in &catalog.forwarding {
        if !live_tools.contains(&scenario.tool) {
            return Err(format!(
                "forwarding scenario {name} selects unknown tool {}",
                scenario.tool
            ));
        }
        let result = owner.execute(&scenario.tool, &scenario.arguments).await;
        forwarding.push(json!({"name":name,"tool":scenario.tool,"arguments":scenario.arguments,"result":result_json(result)}));
    }

    let success_names: BTreeSet<_> = successes
        .iter()
        .filter_map(|v| v["tool"].as_str())
        .collect();
    let success_uncollected: Vec<_> = all_defs
        .iter()
        .filter(|d| !success_names.contains(d.name.as_str()))
        .map(|d| {
            let reason = catalog
                .success_uncollected_l3
                .get(&d.name)
                .map(String::as_str)
                .unwrap_or("selected success scenario did not satisfy its postcondition");
            json!({
                "tool":d.name,
                "facet":"success",
                "status":"uncollected",
                "reason":reason
            })
        })
        .collect();

    if !unsuccessful_attempts.is_empty() {
        return Err(format!(
            "selected tool success scenario did not succeed: {}",
            serde_json::to_string(&unsuccessful_attempts)
                .unwrap_or_else(|_| "<unserializable attempts>".to_string())
        ));
    }

    let uncollected = [no_required_args_uncollected, success_uncollected].concat();
    Ok(json!({
        "missing_arguments":required_missing,
        "permission_denied":permission,
        "successful_calls":successes,
        "unsuccessful_attempts":unsuccessful_attempts,
        "forwarding_failures":forwarding,
        "uncollected":uncollected
    }))
}

async fn collect_mcp_protocol() -> Result<Value, String> {
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (client_stream, server_stream) = duplex(16 * 1024);
    let (client_read, client_write) = tokio::io::split(client_stream);
    let (server_read, mut server_write) = tokio::io::split(server_stream);
    let transcript = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
    let server_transcript = transcript.clone();
    let server = tokio::spawn(async move {
        let mut lines = BufReader::new(server_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let request: Value = serde_json::from_str(&line).expect("local MCP request JSON");
            server_transcript.lock().await.push(request.clone());
            let Some(id) = request.get("id").cloned() else {
                continue;
            };
            let result = match request["method"].as_str() {
                Some("initialize") => {
                    json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"baseline-local","version":"1"}})
                }
                Some("tools/list") => {
                    json!({"tools":[{"name":"echo","description":"local echo","inputSchema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}}]})
                }
                Some("tools/call") => {
                    json!({"content":[{"type":"text","text":"local protocol success"}],"isError":false})
                }
                other => panic!("unexpected local MCP method: {other:?}"),
            };
            let mut response =
                serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"result":result})).unwrap();
            response.push(b'\n');
            server_write.write_all(&response).await.unwrap();
        }
    });
    let connection = opencrab_mcp::McpConnection::new(Box::new(client_write), client_read);
    connection
        .initialize()
        .await
        .map_err(|e| format!("local MCP initialize: {e}"))?;
    let tools = connection
        .list_tools()
        .await
        .map_err(|e| format!("local MCP tools/list: {e}"))?;
    let call = connection
        .call_tool("echo", json!({"value":"baseline"}))
        .await
        .map_err(|e| format!("local MCP tools/call: {e}"))?;
    drop(connection);
    server
        .await
        .map_err(|e| format!("local MCP server task: {e}"))?;
    let transcript = transcript.lock().await.clone();
    Ok(json!({
        "transport":"tokio in-memory duplex (no subprocess, network, credential, or external service)",
        "client_requests":transcript,
        "observed_tools":tools.into_iter().map(|t| json!({"name":t.name,"description":t.description,"input_schema":t.input_schema})).collect::<Vec<_>>(),
        "successful_call":{"text":call.text,"is_error":call.is_error}
    }))
}

fn coverage(http: &Value, tools: &Value) -> Value {
    let mut by_status = BTreeMap::<String, usize>::new();
    let mut rejection_observed = 0;
    let mut effect_observed = 0;
    let mut effect_uncollected = 0;
    if let Some(probes) = http["probes"].as_array() {
        for probe in probes {
            let status = probe
                .pointer("/response/status")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            *by_status.entry(format!("{}xx", status / 100)).or_default() += 1;
            if matches!(
                probe["selection"].as_str(),
                Some("malformed_json_rejection" | "missing_resource_rejection")
            ) && (400..500).contains(&status)
            {
                rejection_observed += 1;
            }
            match probe["effect"]["status"].as_str() {
                Some("observed") => effect_observed += 1,
                Some("uncollected") => effect_uncollected += 1,
                _ => {}
            }
        }
    }
    let uncollected = http["uncollected"].as_array();
    let http_uncollected = uncollected.map_or(0, |items| {
        items
            .iter()
            .filter(|item| item["status"] == "uncollected")
            .count()
    });
    let http_not_applicable = uncollected.map_or(0, |items| {
        items
            .iter()
            .filter(|item| item["status"] == "not_applicable")
            .count()
    });
    let tool_postconditions = tools["successful_calls"].as_array().map_or(0, |items| {
        items
            .iter()
            .filter(|item| !item["postcondition"].is_null() || !item["effect"].is_null())
            .count()
    });
    json!({
        "http_observed_status_classes":by_status,
        "http_facets":{
            "valid_rejections":rejection_observed,
            "effects_observed":effect_observed,
            "effects_uncollected":effect_uncollected,
            "uncollected":http_uncollected,
            "not_applicable":http_not_applicable
        },
        "tool_effects_observed":tool_postconditions,
        "tool_success_uncollected_count":tools["uncollected"].as_array().map_or(0, |items| items.iter().filter(|item| item["facet"] == "success").count()),
        "claim":"Only nonempty observations satisfying their facet predicate are fixed by this artifact; every uncollected or not_applicable entry is an explicit non-claim."
    })
}

pub async fn capture(l1_path: &Path, scenario_path: &Path) -> Result<Value, String> {
    let capture_profile = capture_profile()?;
    let l1 = read_json(l1_path)?;
    let (scenarios, catalog) = read_scenarios(scenario_path)?;
    if catalog.schema_version != 1 {
        return Err("scenario catalog schema_version must be 1".to_string());
    }
    let http = collect_http(&l1, &catalog.http).await?;
    let visibility = collect_visibility(&catalog.tool_visibility)?;
    let tools = collect_tool_execution(&catalog.tool_execution).await?;
    let mcp = collect_mcp_protocol().await?;
    let coverage = coverage(&http, &tools);
    let mut artifact = json!({
        "schema_version":1,
        "capture_profile":capture_profile,
        "source":{"l1":l1_path.file_name().and_then(|s|s.to_str()).unwrap_or("opencrab-l1.json"),"scenario_catalog":scenario_path.file_name().and_then(|s|s.to_str()).unwrap_or("scenarios.json")},
        "normalization":[
            "content-length response header omitted",
            "timestamp-valued *_at fields and timestamp-valued date_from/date_to fields replaced with <timestamp>",
            "numeric duration_ms/latency_ms replaced with <duration>",
            "floating score values rounded to 12 decimal places to remove platform-level SQLite FTS noise",
            "collector-owned temporary workspace roots replaced with <workspace>",
            "UUID strings and subtask-UUID strings replaced with <uuid> markers",
            "implementation-generated unit-*/core-* identifiers replaced with field-specific markers; fixed fixture identifiers remain literal"
        ],
        "scenario_catalog":scenarios,
        "http":http,
        "tool_visibility":visibility,
        "tool_execution":tools,
        "mcp_protocol":mcp,
        "coverage":coverage
    });
    normalize(&mut artifact);
    Ok(artifact)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_difference(left: &Value, right: &Value, path: &str) -> Option<String> {
        match (left, right) {
            (Value::Object(left), Value::Object(right)) => {
                for key in left.keys().chain(right.keys()) {
                    if left.get(key) != right.get(key) {
                        let child = format!("{path}/{key}");
                        return match (left.get(key), right.get(key)) {
                            (Some(left), Some(right)) => {
                                first_difference(left, right, &child).or(Some(child))
                            }
                            _ => Some(child),
                        };
                    }
                }
                None
            }
            (Value::Array(left), Value::Array(right)) => left
                .iter()
                .zip(right)
                .enumerate()
                .find_map(|(index, (left, right))| {
                    (left != right).then(|| {
                        first_difference(left, right, &format!("{path}/{index}"))
                            .unwrap_or_else(|| format!("{path}/{index}"))
                    })
                })
                .or_else(|| (left.len() != right.len()).then(|| format!("{path}/length"))),
            _ => (left != right).then(|| path.to_string()),
        }
    }

    fn baseline_paths() -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("server crate must be below the repository root");
        (
            repository.join("baseline/l1/opencrab-l1.json"),
            repository.join("baseline/l2/scenarios.json"),
            repository.join("baseline/l2/opencrab-l2.json"),
        )
    }

    #[test]
    fn production_builder_and_capture_visibility_are_identical() {
        let (_, scenarios_path, _) = baseline_paths();
        let (_, catalog) = read_scenarios(&scenarios_path).expect("read scenario catalog");
        let visibility = collect_visibility(&catalog.tool_visibility)
            .expect("collect through production executor builder");
        let rows = visibility["scenarios"].as_array().expect("visibility rows");
        let mcp_count = |name: &str| {
            rows.iter()
                .find(|row| row["name"] == name)
                .and_then(|row| row["visible_tools"].as_array())
                .expect("named visibility row")
                .iter()
                .filter(|tool| {
                    tool.as_str()
                        .is_some_and(|tool_name| tool_name.starts_with("mcp__"))
                })
                .count()
        };
        assert_eq!(mcp_count("owner_depth0_all_features"), 2);
        assert_eq!(mcp_count("owner_depth1_subengine"), 0);
        assert_eq!(mcp_count("owner_depth2_cap"), 0);
    }

    #[test]
    fn live_routes_and_tools_match_the_scenario_catalog() {
        let (l1_path, scenarios_path, _) = baseline_paths();
        let l1 = read_json(&l1_path).expect("read checked L1 artifact");
        let production_routes = serde_json::to_value(crate::production_route_inventory())
            .expect("serialize production route inventory");
        assert_eq!(l1["http"]["routes"], production_routes);

        let (_, catalog) = read_scenarios(&scenarios_path).expect("read scenario catalog");
        let owner = build_executor(
            opencrab_actions::CallerIdentity::Owner,
            0,
            true,
            None,
            "default",
        );
        let live_tools: BTreeSet<_> = owner
            .list_tools()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        let selected_tools: BTreeSet<_> = catalog
            .tool_execution
            .success_arguments
            .keys()
            .chain(catalog.tool_execution.success_uncollected_l3.keys())
            .cloned()
            .collect();
        assert_eq!(live_tools, selected_tools);
    }

    #[tokio::test]
    async fn observed_facets_have_valid_statuses_and_nonempty_effects() {
        let (l1_path, scenarios_path, _) = baseline_paths();
        let artifact = capture(&l1_path, &scenarios_path)
            .await
            .expect("capture L2 artifact");
        let probes = artifact["http"]["probes"].as_array().expect("HTTP probes");
        for probe in probes {
            if matches!(
                probe["selection"].as_str(),
                Some("malformed_json_rejection" | "missing_resource_rejection")
            ) {
                let status = probe["response"]["status"]
                    .as_u64()
                    .expect("rejection status");
                assert!((400..500).contains(&status));
            }
            if probe["effect"]["status"] == "observed" {
                assert_ne!(probe["effect"]["before"], probe["effect"]["after"]);
            }
        }

        let successful_calls = artifact["tool_execution"]["successful_calls"]
            .as_array()
            .expect("successful tool calls");
        for tool in &[
            "spawn_subtask",
            "cancel_subtask",
            "steer_subtask",
            "report_progress",
        ] {
            let row = successful_calls
                .iter()
                .find(|row| row["tool"] == *tool)
                .expect("subtask effect row");
            assert!(!row["effect"].is_null());
        }
    }

    #[tokio::test]
    async fn capture_is_deterministic_and_matches_the_checked_artifact() {
        let (l1_path, scenarios_path, artifact_path) = baseline_paths();
        let first = capture(&l1_path, &scenarios_path)
            .await
            .expect("first L2 capture");
        let second = capture(&l1_path, &scenarios_path)
            .await
            .expect("second L2 capture");
        if first != second {
            panic!(
                "successive captures differ at {}",
                first_difference(&first, &second, "").unwrap_or_else(|| "unknown".to_string())
            );
        }

        let mut bytes = serde_json::to_vec_pretty(&first).expect("serialize fresh L2 artifact");
        bytes.push(b'\n');
        let checked_bytes = fs::read(artifact_path).expect("read checked L2 artifact");
        let checked: Value =
            serde_json::from_slice(&checked_bytes).expect("parse checked L2 artifact");
        if first != checked {
            let uncollected_effects = |artifact: &Value| {
                artifact["http"]["probes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|probe| probe["effect"]["status"] == "uncollected")
                    .filter_map(|probe| probe["name"].as_str().map(ToOwned::to_owned))
                    .collect::<Vec<_>>()
            };
            let difference =
                first_difference(&first, &checked, "").unwrap_or_else(|| "unknown".to_string());
            panic!(
                "fresh capture differs from checked artifact at {difference}: fresh={:?}, checked={:?}; fresh uncollected effects={:?}; checked={:?}",
                first.pointer(&difference),
                checked.pointer(&difference),
                uncollected_effects(&first),
                uncollected_effects(&checked),
            );
        }
        assert_eq!(bytes, checked_bytes, "checked artifact formatting differs");
        let text = String::from_utf8(bytes).expect("artifact is UTF-8");
        for host_path in [
            std::env::temp_dir(),
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        ] {
            let host_path = host_path.to_string_lossy();
            assert!(
                !text.contains(host_path.as_ref()),
                "artifact contains host path {host_path:?}"
            );
        }
        for host_marker in [
            "/tmp",
            "/private/tmp",
            "/var/folders/",
            "/Users/",
            "/Volumes/",
            "/home/",
            "Linux",
            "Darwin",
            "macOS",
            "Windows",
        ] {
            assert!(
                !text.contains(host_marker),
                "artifact contains host marker {host_marker:?}"
            );
        }
        assert!(
            !text.contains(":\\\\"),
            "artifact contains a Windows absolute path"
        );
        assert_eq!(
            first["scenario_catalog"]["http"]["query_suffixes"]
                ["/api/agents/{id}/import/sync/status"],
            "?source_dir={collector_workspace}/import-source&include_daily_logs=false"
        );
        let sync_status = first["http"]["probes"]
            .as_array()
            .expect("HTTP probes")
            .iter()
            .find(|probe| probe["name"] == "get___api_agents_id_import_sync_status__normal")
            .expect("import sync status probe");
        assert_eq!(sync_status["response"]["status"], 200);
        assert_eq!(
            sync_status["response"]["body"]["source_dir"],
            "<workspace>/import-source"
        );
        assert_eq!(
            first["capture_profile"]["build"]["required_cargo_features"],
            json!(["discord", "nostr", "web"])
        );
        let diagnostic_probe = |name: &str| {
            first["http"]["probes"]
                .as_array()
                .expect("HTTP probes")
                .iter()
                .find(|probe| probe["name"] == name)
                .expect("diagnostic probe")
        };
        for name in [
            "get___api_llm_codex_diagnostics__normal",
            "get___api_llm_cursor_diagnostics__normal",
        ] {
            let body = &diagnostic_probe(name)["response"]["body"];
            assert_eq!(body["configured_path"], "<workspace>/baseline-command");
            assert_eq!(body["resolved_path"], "<workspace>/baseline-command");
            assert_eq!(body["version"], "baseline-cli 1.0");
            assert!(body["error"].is_null());
        }
        let shell_call = first["tool_execution"]["successful_calls"]
            .as_array()
            .expect("successful tool calls")
            .iter()
            .find(|call| call["tool"] == "execute_shell")
            .expect("execute_shell call");
        assert_eq!(
            shell_call["arguments"]["command"],
            "<workspace>/baseline-command"
        );
        assert_eq!(shell_call["result"]["data"]["stdout"], "baseline-shell");
        assert!(text.contains("2026-01-01"));
    }
}
