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
    sync::Arc,
};

use async_trait::async_trait;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use opencrab_core::engine::ActionExecutor;
use opencrab_gateway::GatewayActions;
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::{create_router, system_actions::SystemGatewayActions, test_app_state, AppState};

const AGENT_ID: &str = "baseline-agent";
const SESSION_ID: &str = "baseline-session";
const TOOL_SESSION_ID: &str = "web-baseline-agent-conversation";
const MISSING: &str = "baseline-missing";

fn read_json(path: &Path) -> Result<Value, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))
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
            }
        }
        Value::String(text) if uuid::Uuid::parse_str(text).is_ok() => {
            *text = "<uuid>".to_string();
        }
        Value::String(text) => {
            if text
                .strip_prefix("subtask-")
                .is_some_and(|suffix| uuid::Uuid::parse_str(suffix).is_ok())
            {
                *text = "subtask-<uuid>".to_string();
            }
        }
        _ => {}
    }
}

fn seeded_state() -> Result<AppState, String> {
    let mut state = test_app_state();
    let workspace = std::env::temp_dir().join(format!(
        "opencrab-baseline-l2-workspace-{}",
        std::process::id()
    ));
    fs::create_dir_all(&workspace).map_err(|e| format!("create baseline workspace: {e}"))?;
    fs::write(workspace.join("baseline.txt"), b"baseline\n")
        .map_err(|e| format!("seed baseline workspace: {e}"))?;
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

fn concrete_path(template: &str, missing: bool) -> String {
    let id = if missing { MISSING } else { AGENT_ID };
    template
        .replace("{id}", id)
        .replace("{sid}", if missing { "999999" } else { "1" })
        .replace(
            "{skill_id}",
            if missing { MISSING } else { "baseline-skill" },
        )
        .replace(
            "{preset_id}",
            if missing { MISSING } else { "baseline-preset" },
        )
        .replace(
            "{entry_id}",
            if missing { MISSING } else { "baseline-memory" },
        )
        .replace(
            "{co_agent_id}",
            if missing { MISSING } else { "baseline-peer" },
        )
        .replace(
            "{channel_id}",
            if missing { MISSING } else { "baseline-channel" },
        )
        // The handler currently passes this path field to DB functions that key by row id
        // despite the public placeholder being named `user_id`; capture that behavior as-is.
        .replace(
            "{user_id}",
            if missing {
                MISSING
            } else {
                "baseline-trusted-row"
            },
        )
        .replace("{command}", "baseline-command")
        .replace("{source}", "baseline-source")
        .replace("{name}", "baseline")
        .replace("{*path}", "baseline.txt")
}

fn concrete_uri(template: &str, missing: bool) -> String {
    let path = concrete_path(template, missing);
    if template == "/api/agents/{id}/import/sync/status" {
        format!("{path}?source_dir=/tmp")
    } else {
        path
    }
}

fn nominal_body(method: &str, path: &str) -> Option<Value> {
    if !matches!(method, "POST" | "PUT" | "PATCH") {
        return None;
    }
    let body = match path {
        "/api/agents" => json!({"name":"Created Baseline Agent","persona_name":"Created"}),
        "/api/agents/{id}" => json!({"name":"Baseline Agent","persona_name":"Baseline"}),
        "/api/agents/{id}/allowed-commands" => json!({"command":"printf"}),
        "/api/agents/{id}/channel-configs" => {
            json!({"channel_id":"baseline-new-channel","guild_id":"baseline-guild","channel_name":"new","readable":true,"writable":true,"whitelisted":true})
        }
        "/api/agents/{id}/co-agents" => json!({"co_agent_id":"baseline-new-peer"}),
        "/api/agents/{id}/discord" => {
            if method == "PUT" {
                json!({"bot_token":"baseline-not-a-credential","owner_discord_id":"baseline-owner"})
            } else {
                json!({"owner_discord_id":"baseline-owner-2"})
            }
        }
        "/api/agents/{id}/mcp" => {
            json!({"name":"baseline","command":"false","args":[],"env":{},"enabled":false,"trusted_only":false})
        }
        "/api/agents/{id}/mcp/{name}/enabled" => json!({"enabled":false}),
        "/api/agents/{id}/memory/index/config" => json!({"enabled":false}),
        "/api/agents/{id}/memory/index/merge" => {
            json!({"source_topic_id":"missing-a","target_topic_id":"missing-b"})
        }
        "/api/agents/{id}/memory/search" => json!({"query":"baseline","limit":10}),
        "/api/agents/{id}/nostr" => json!({"enabled":false}),
        "/api/agents/{id}/nostr-relay" => json!({"enabled":false}),
        "/api/agents/{id}/schedules" => {
            json!({"session_id":format!("nostr-{AGENT_ID}"),"cron_expr":"0 1 * * *","timezone":"Asia/Tokyo","message":"baseline","enabled":false})
        }
        "/api/agents/{id}/skills" => {
            json!({"name":"Baseline Skill","description":"baseline","situation_pattern":"baseline","guidance":"baseline"})
        }
        "/api/agents/{id}/skills/{skill_id}" => {
            json!({"name":"Baseline Skill","description":"baseline","situation_pattern":"baseline","guidance":"baseline"})
        }
        "/api/agents/{id}/skills/{skill_id}/toggle" => json!({"active":false}),
        "/api/agents/{id}/soul/presets" => {
            json!({"preset_name":"Baseline","persona_name":"Baseline"})
        }
        "/api/agents/{id}/trusted-users" => {
            json!({"platform":"web","user_id":"baseline-new-user","display_name":"Baseline New User","permission":"user"})
        }
        "/api/agents/{id}/trusted-users/{user_id}" => json!({"display_name":"Renamed"}),
        "/api/agents/{id}/workspace/{*path}" => json!({"content":"baseline write\n"}),
        "/api/hooks/{source}" => json!({"type":"baseline.event","data":{"id":"baseline-event"}}),
        "/api/import/scan" => json!({"path":"."}),
        "/api/import/execute" => json!({"confirmed":false}),
        "/api/llm/model-pricing" => {
            json!({"provider":"baseline","model":"model","input_price_per_1m":0.0,"output_price_per_1m":0.0,"context_window":4096})
        }
        "/api/llm/providers/{name}" => json!({"enabled":false}),
        "/api/schedules/{sid}" => json!({"enabled":false}),
        "/api/sessions" => json!({"theme":"Baseline","participant_ids":[]}),
        "/api/sessions/{id}/mentor" => json!({"content":"baseline"}),
        "/api/sessions/{id}/messages" => json!({"agent_id":AGENT_ID,"content":"baseline"}),
        "/api/system/log-level" => json!({"log_level":"info"}),
        "/api/voice/config" => json!({}),
        _ => json!({}),
    };
    Some(body)
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
    headers.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
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

fn route_is_l3(method: &str, path: &str) -> Option<&'static str> {
    match (method, path) {
        ("POST", "/api/agents/{id}/discord/start") => Some("would connect to Discord"),
        ("POST", "/api/agents/{id}/nostr/start") => Some("would connect to Nostr relays"),
        ("POST", "/api/agents/{id}/nostr/generate") => {
            Some("successful vanity-key generation requires the operator's nostaro executable")
        }
        ("POST", "/api/agents/{id}/mcp/{name}/test") => Some("operator supplied subprocess"),
        ("POST", "/api/llm/providers/{name}/test") => Some("provider network endpoint"),
        ("POST", "/api/sessions/{id}/messages") => {
            Some("successful branch requires an LLM provider")
        }
        ("POST", "/api/agents/{id}/web/send") => Some("successful branch requires an LLM provider"),
        ("GET", "/api/agents/{id}/web/stream") => {
            Some("successful response is an intentionally non-terminating SSE stream")
        }
        (
            "POST",
            "/api/agents/{id}/daily-log-index/rebuild" | "/api/agents/{id}/daily-log-index/run",
        ) => Some("successful branch schedules LLM-backed indexing"),
        ("POST", "/api/agents/{id}/memory/index" | "/api/agents/{id}/memory/index/rebuild") => {
            Some("successful branch schedules LLM-backed indexing")
        }
        ("POST", "/api/agents/{id}/import/sync") => Some("depends on an operator import source"),
        ("POST", "/api/import/scan" | "/api/import/execute") => {
            Some("depends on an operator-selected external workspace")
        }
        _ => None,
    }
}

async fn collect_http(l1: &Value) -> Result<Value, String> {
    let routes = l1
        .pointer("/http/routes")
        .and_then(Value::as_array)
        .ok_or_else(|| "L1 /http/routes is missing or not an array".to_string())?;
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
            let stem = format!(
                "{}__{}",
                method.to_ascii_lowercase(),
                path.replace('/', "_").replace(['{', '}', '*'], "")
            );

            if let Some(reason) = route_is_l3(method, path) {
                uncollected.push(json!({
                    "name": format!("{stem}__normal"),
                    "method": method,
                    "path": path,
                    "branch": "normal",
                    "status": "uncollected",
                    "reason": reason,
                    "level": "L3"
                }));
            } else {
                let uri = concrete_uri(path, false);
                let body_value = nominal_body(method, path);
                let body = body_value
                    .as_ref()
                    .map(serde_json::to_vec)
                    .transpose()
                    .map_err(|e| format!("serialize {stem} normal body: {e}"))?;
                let response = request_once(
                    seeded_state()?,
                    &format!("{stem}__normal"),
                    method,
                    &uri,
                    body,
                    body_value.as_ref().map(|_| "application/json"),
                )
                .await?;
                probes.push(json!({
                    "name": format!("{stem}__normal"),
                    "selection": "normal_or_local_precondition_branch",
                    "request": {"method":method,"uri":uri,"body":body_value},
                    "response": response
                }));
            }

            if method == "GET" && path == "/api/agents/{id}/web/stream" {
                uncollected.push(json!({
                    "name": format!("{stem}__reject_or_absent"),
                    "method": method,
                    "path": path,
                    "branch": "resource_absence_or_empty_state",
                    "status": "uncollected",
                    "reason": "collector does not substitute a finite body for the route's non-terminating SSE contract",
                    "level": "L3"
                }));
                continue;
            }

            let (uri, body, content_type, selection) = if matches!(method, "POST" | "PUT" | "PATCH")
            {
                (
                    concrete_uri(path, false),
                    Some(b"{".to_vec()),
                    Some("application/json"),
                    "malformed_json_rejection",
                )
            } else {
                (
                    concrete_uri(path, true),
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
            probes.push(json!({
                "name": format!("{stem}__reject_or_absent"),
                "selection": selection,
                "request": {"method":method,"uri":uri,"body_utf8": if content_type.is_some() { Value::String("{".to_string()) } else { Value::Null }},
                "response": response
            }));
        }
    }
    Ok(json!({
        "source_route_count": routes.len(),
        "probes": probes,
        "uncollected": uncollected,
    }))
}

fn action_context(caller: opencrab_actions::CallerIdentity) -> opencrab_actions::ActionContext {
    let root =
        std::env::temp_dir().join(format!("opencrab-baseline-l2-tools-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create baseline tool workspace");
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
    }
    opencrab_actions::ActionContext {
        agent_id: AGENT_ID.to_string(),
        agent_name: "Baseline Agent".to_string(),
        session_id: Some(TOOL_SESSION_ID.to_string()),
        db,
        workspace: Arc::new(workspace),
        last_metrics_id: Arc::new(std::sync::Mutex::new(None)),
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

fn dispatcher(shell_enabled: bool) -> opencrab_actions::ActionDispatcher {
    let mut dispatcher = opencrab_actions::ActionDispatcher::new();
    opencrab_actions::register_tools_from_config(
        &opencrab_actions::ToolsConfig {
            enabled: shell_enabled,
            shell: shell_enabled.then(|| opencrab_actions::ShellToolConfig {
                allowed_commands: vec!["printf".to_string()],
                ..Default::default()
            }),
        },
        &mut dispatcher,
    );
    dispatcher
}

fn tool_names(executor: &opencrab_actions::BridgedExecutor) -> Vec<String> {
    let mut names: Vec<_> = executor.list_tools().into_iter().map(|d| d.name).collect();
    names.sort();
    names
}

fn build_executor(
    caller: opencrab_actions::CallerIdentity,
    depth: u32,
    shell_enabled: bool,
    allowlist: Option<Vec<String>>,
    caller_is_trusted_for_mcp: bool,
) -> opencrab_actions::BridgedExecutor {
    let root_gateway: Arc<dyn GatewayActions> = Arc::new(SystemGatewayActions::new(
        seeded_tool_state(),
        None,
        None,
        None,
    ));
    let gateway: Arc<dyn GatewayActions> = if depth == 0 {
        root_gateway
    } else {
        Arc::new(opencrab_actions::SubEngineGatewayActions::new(root_gateway))
    };
    let mcp: Arc<dyn GatewayActions> = Arc::new(opencrab_mcp::McpToolProvider::new(
        mcp_servers(),
        caller_is_trusted_for_mcp,
    ));
    opencrab_actions::BridgedExecutor::new(dispatcher(shell_enabled), action_context(caller))
        .with_depth(depth)
        .with_gateway_actions(gateway)
        .with_mcp_actions(mcp)
        .with_tool_allowlist(allowlist)
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
) -> opencrab_actions::BridgedExecutor {
    let root_gateway: Arc<dyn GatewayActions> = Arc::new(SystemGatewayActions::new(
        seeded_tool_state(),
        None,
        Some(registry),
        None,
    ));
    let gateway: Arc<dyn GatewayActions> = if depth == 0 {
        root_gateway
    } else {
        Arc::new(opencrab_actions::SubEngineGatewayActions::new(root_gateway))
    };
    let mut ctx = action_context(opencrab_actions::CallerIdentity::Owner);
    ctx.session_id = Some(session_id.to_string());
    opencrab_actions::BridgedExecutor::new(dispatcher(false), ctx)
        .with_depth(depth)
        .with_gateway_actions(gateway)
}

fn collect_visibility() -> Value {
    use opencrab_actions::CallerIdentity;
    let cases = vec![
        (
            "owner_depth0_all_features",
            CallerIdentity::Owner,
            0,
            true,
            None,
            true,
        ),
        (
            "coagent_owner_equivalent",
            CallerIdentity::CoAgent {
                agent_id: "baseline-peer".to_string(),
            },
            0,
            true,
            None,
            true,
        ),
        (
            "trusted_user",
            CallerIdentity::TrustedUser,
            0,
            true,
            None,
            true,
        ),
        (
            "untrusted_agent",
            CallerIdentity::Agent,
            0,
            true,
            None,
            false,
        ),
        (
            "owner_depth1_subengine",
            CallerIdentity::Owner,
            1,
            true,
            None,
            true,
        ),
        (
            "owner_depth2_cap",
            CallerIdentity::Owner,
            2,
            true,
            None,
            true,
        ),
        (
            "owner_shell_disabled",
            CallerIdentity::Owner,
            0,
            false,
            None,
            true,
        ),
        (
            "owner_cross_slot_allowlist",
            CallerIdentity::Owner,
            0,
            true,
            Some(vec![
                "get_system_info".to_string(),
                "list_allowed_commands".to_string(),
                "mcp__public_local__echo".to_string(),
            ]),
            true,
        ),
        (
            "untrusted_mcp_trusted_server_hidden",
            CallerIdentity::Agent,
            0,
            false,
            None,
            false,
        ),
    ];
    let rows: Vec<_> = cases
        .into_iter()
        .map(|(name, caller, depth, shell, allowlist, mcp_trusted)| {
            let caller_name = match &caller {
                CallerIdentity::Owner => "owner",
                CallerIdentity::Agent => "agent",
                CallerIdentity::TrustedUser => "trusted_user",
                CallerIdentity::CoAgent { .. } => "co_agent",
            };
            let executor = build_executor(caller, depth, shell, allowlist.clone(), mcp_trusted);
            json!({
                "name":name,
                "dimensions": {"caller":caller_name,"depth":depth,"shell_enabled":shell,"allowlist":allowlist,"mcp_caller_is_trusted":mcp_trusted},
                "visible_tools": tool_names(&executor)
            })
        })
        .collect();
    json!({"scenarios":rows})
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

fn result_json(mut result: opencrab_core::ActionResult) -> Value {
    normalize(&mut result.data);
    json!({"success":result.success,"data":result.data,"error":result.error})
}

async fn collect_tool_execution() -> Result<Value, String> {
    use opencrab_actions::CallerIdentity;
    let owner = build_executor(CallerIdentity::Owner, 0, true, None, true);
    let all_defs = owner.list_tools();
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

    let agent = build_executor(CallerIdentity::Agent, 0, true, None, false);
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

    let success_cases = [
        ("get_system_info", json!({})),
        ("declare_done", json!({"reason":"baseline complete"})),
        (
            "generate_inner_voice",
            json!({"thought":"baseline thought"}),
        ),
        (
            "update_impression",
            json!({"target_id":"baseline-peer","target_name":"Baseline Peer","agreement":"neutral"}),
        ),
        ("ws_mkdir", json!({"path":"captured"})),
        (
            "ws_write",
            json!({"path":"captured/file.txt","content":"baseline"}),
        ),
        ("ws_read", json!({"path":"captured/file.txt"})),
        ("ws_list", json!({"path":"captured"})),
        (
            "ws_edit",
            json!({"path":"captured/file.txt","old_string":"baseline","new_string":"captured"}),
        ),
        ("ws_delete", json!({"path":"captured/file.txt"})),
        (
            "learn_from_experience",
            json!({"experience":"baseline run","outcome":"success","lesson":"capture real results","skill_name":"Experience Skill"}),
        ),
        (
            "learn_from_peer",
            json!({"peer_name":"Baseline Peer","observed_pattern":"records fixtures","lesson":"make preconditions explicit","skill_name":"Peer Skill"}),
        ),
        (
            "reflect_and_learn",
            json!({"reflection":"baseline reflection","insights":["observed"],"action_items":["preserve"]}),
        ),
        ("search_my_history", json!({"query":"baseline","limit":5})),
        (
            "summarize_and_save",
            json!({"content":"baseline summary","filename":"captured/summary.txt","summary_type":"note"}),
        ),
        (
            "create_my_skill",
            json!({"name":"Captured Skill","description":"captured","situation_pattern":"baseline","guidance":"preserve","actions":["get_system_info"]}),
        ),
        ("retire_my_skill", json!({"name":"Seed Skill"})),
        ("restore_my_skill", json!({"name":"Seed Skill"})),
        ("read_skill", json!({"name":"Seed Skill"})),
        ("browse_memory_index", json!({"max_depth":3})),
        ("retrieve_memory_nodes", json!({"node_ids":["t1"]})),
        ("search_memory_index", json!({"query":"Baseline","limit":5})),
        (
            "tag_topic",
            json!({"topic_id":"t1","tags":["Baseline Tag","Merge From"]}),
        ),
        ("untag_topic", json!({"topic_id":"t1","tag":"Baseline Tag"})),
        (
            "merge_tags",
            json!({"from":"Merge From","into":"Merge Into"}),
        ),
        (
            "survey_my_history",
            json!({"granularity":"day","max_buckets":5}),
        ),
        ("read_my_history", json!({"session_id":SESSION_ID})),
        (
            "record_memory_unit",
            json!({"from_id":1,"to_id":2,"title":"Recorded Unit","summary":"captured"}),
        ),
        ("retract_memory_unit", json!({"unit_id":"u2"})),
        (
            "plan_next_memory_window",
            json!({"next_from_id":1,"window_size":10,"note":"baseline"}),
        ),
        (
            "record_memory_core",
            json!({"axis":"Recorded Core","body":"captured principle","sources":["u1"]}),
        ),
        (
            "update_memory_core",
            json!({"core_id":"m1","axis":"Updated Core","body":"updated principle","sources":["u1"]}),
        ),
        ("retract_memory_core", json!({"core_id":"m2"})),
        (
            "select_llm",
            json!({"model_alias":"baseline:model","reason":"compatibility capture","purpose":"baseline","duration":"this_turn"}),
        ),
        (
            "evaluate_response",
            json!({"evaluation":"baseline evaluation","quality_score":0.75,"task_success":true,"tags":["baseline"]}),
        ),
        ("analyze_llm_usage", json!({"period":"all"})),
        (
            "recall_model_experiences",
            json!({"include_notes":true,"evaluation_limit":5}),
        ),
        (
            "save_model_insight",
            json!({"situation":"baseline","observation":"captured behavior","recommendation":"preserve","model":"baseline:model","tags":["compatibility"]}),
        ),
        (
            "update_instructions",
            json!({"instructions":"baseline updated instructions","reason":"compatibility capture"}),
        ),
        (
            "open_task",
            json!({"goal":"baseline goal","contract":"captured"}),
        ),
        (
            "update_task_contract",
            json!({"goal":"baseline updated goal","contract":"captured result"}),
        ),
        (
            "record_task_progress",
            json!({"content":"baseline progress","kind":"progress"}),
        ),
        ("get_task", json!({})),
        (
            "close_task",
            json!({"status":"done","summary":"baseline done"}),
        ),
        (
            "execute_shell",
            json!({"command":"printf","args":["baseline-shell"]}),
        ),
        (
            "configure_llm_provider",
            json!({"provider":"openai","enabled":false}),
        ),
        ("manage_allowed_commands", json!({"action":"list"})),
        ("configure_nostr", json!({"enabled":false})),
        (
            "configure_self",
            json!({"personality":"baseline configured"}),
        ),
        ("configure_mcp_server", json!({"action":"list"})),
        ("nostr_list_keys", json!({})),
        ("rebuild_memory_index", json!({})),
        (
            "update_memory_index_config",
            json!({"batch_size":20,"threshold":100}),
        ),
        ("add_allowed_command", json!({"command":"baseline_cmd"})),
        ("list_allowed_commands", json!({})),
        ("remove_allowed_command", json!({"command":"baseline_cmd"})),
        (
            "create_skill",
            json!({"name":"Gateway Skill","description":"captured","guidance":"preserve"}),
        ),
        (
            "update_heartbeat_instructions",
            json!({"scope":"agent","instructions":"baseline heartbeat updated","reason":"compatibility capture"}),
        ),
        ("read_heartbeat_instructions", json!({"scope":"agent"})),
        ("get_my_nostr_relay", json!({})),
        (
            "set_my_nostr_relay",
            json!({"enabled":false,"webhook_url":""}),
        ),
        ("get_my_heartbeat", json!({})),
        (
            "set_my_heartbeat",
            json!({"enabled":false,"interval_secs":3600}),
        ),
        ("get_my_schedules", json!({})),
        (
            "set_my_schedule",
            json!({"cron_expr":"0 9 * * *","message":"baseline schedule","timezone":"UTC","enabled":true}),
        ),
        (
            "update_my_schedule",
            json!({"id":1,"message":"baseline schedule updated","enabled":false}),
        ),
        ("delete_my_schedule", json!({"id":1})),
        ("get_default_webhook", json!({})),
        ("get_default_subtask_webhook", json!({})),
        (
            "set_default_subtask_webhook",
            json!({"scope":"agent","kind":"discord","url":"https://discord.com/api/webhooks/1/baseline","enabled":false}),
        ),
        (
            "list_subtask_webhooks",
            json!({"scope":"all","include_disabled":true}),
        ),
        (
            "set_default_webhook",
            json!({"scope":"agent","family":"tool","url":"https://discord.com/api/webhooks/1/baseline","enabled":false}),
        ),
        (
            "list_webhooks",
            json!({"scope":"all","include_disabled":true}),
        ),
        ("mcp__public_local__echo", json!({"value":"baseline"})),
        ("mcp__trusted_local__echo", json!({"value":"baseline"})),
    ];
    let mut successes = Vec::new();
    let mut unsuccessful_attempts = Vec::new();
    for (tool, arguments) in success_cases {
        let result = owner.execute(tool, &arguments).await;
        let result = result_json(result);
        if result["success"] == true {
            successes.push(json!({"tool":tool,"arguments":arguments,"result":result}));
        } else {
            unsuccessful_attempts.push(json!({
                "tool":tool,
                "arguments":arguments,
                "result":result,
                "status":"uncollected",
                "reason":"the selected local success precondition reached the real implementation but did not return success"
            }));
        }
    }

    let subtask_cases = [
        (
            "spawn_subtask",
            TOOL_SESSION_ID,
            0,
            false,
            json!({"task":"baseline delegated task","label":"baseline subtask","timeout_secs":1}),
        ),
        (
            "cancel_subtask",
            TOOL_SESSION_ID,
            0,
            false,
            json!({"subtask_id":"baseline-subtask"}),
        ),
        (
            "steer_subtask",
            TOOL_SESSION_ID,
            0,
            true,
            json!({"subtask_id":"baseline-subtask","message":"baseline steering"}),
        ),
        (
            "report_progress",
            "subtask-baseline-subtask",
            1,
            true,
            json!({"subtask_id":"baseline-subtask","message":"baseline progress"}),
        ),
    ];
    for (tool, session_id, depth, steerable, arguments) in subtask_cases {
        let registry = subtask_fixture_registry(
            "baseline-subtask",
            "subtask-baseline-subtask",
            TOOL_SESSION_ID,
            steerable,
        );
        let executor = build_subtask_fixture_executor(session_id, depth, registry);
        let result = result_json(executor.execute(tool, &arguments).await);
        if result["success"] == true {
            successes.push(json!({"tool":tool,"arguments":arguments,"result":result}));
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

    let forwarding_cases = [
        (
            "mcp_tool_error",
            json!({"value":"baseline","mode":"tool_error"}),
        ),
        (
            "mcp_transport_error",
            json!({"value":"baseline","mode":"transport_error"}),
        ),
    ];
    let mut forwarding = Vec::new();
    for (name, arguments) in forwarding_cases {
        let result = owner.execute("mcp__public_local__echo", &arguments).await;
        forwarding.push(json!({"name":name,"tool":"mcp__public_local__echo","arguments":arguments,"result":result_json(result)}));
    }

    let success_names: BTreeSet<_> = successes
        .iter()
        .filter_map(|v| v["tool"].as_str())
        .collect();
    let success_uncollected: Vec<_> = all_defs
        .iter()
        .filter(|d| !success_names.contains(d.name.as_str()))
        .map(|d| {
            let reason = match d.name.as_str() {
                "nostr_generate_key" => "L3: successful key generation requires the operator's nostaro executable and writes key material outside the temporary workspace",
                "nostr_switch_identity" => "L3: success requires a generated Nostr key and a configured Nostr transport capability",
                "nostr_run" => "L3: success requires an adopted identity plus the operator's nostaro executable and may use live relays",
                "run_my_heartbeat" => "L3: success fires an actual agent turn through a configured transport and LLM provider",
                "request_peer_review" => "L3: success delivers a message through the active transport",
                _ => "the attempted local fixture did not produce a successful result; see unsuccessful_attempts",
            };
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
    if let Some(probes) = http["probes"].as_array() {
        for probe in probes {
            let status = probe
                .pointer("/response/status")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            *by_status.entry(format!("{}xx", status / 100)).or_default() += 1;
        }
    }
    json!({
        "http_observed_status_classes":by_status,
        "http_uncollected_count":http["uncollected"].as_array().map_or(0, Vec::len),
        "tool_success_uncollected_count":tools["uncollected"].as_array().map_or(0, |items| items.iter().filter(|item| item["facet"] == "success").count()),
        "claim":"Only scenarios with captured observations are fixed by this artifact; every uncollected entry is an explicit non-claim."
    })
}

pub async fn capture(l1_path: &Path, scenario_path: &Path) -> Result<Value, String> {
    let l1 = read_json(l1_path)?;
    let scenarios = read_json(scenario_path)?;
    if scenarios["schema_version"] != 1 {
        return Err("scenario catalog schema_version must be 1".to_string());
    }
    let http = collect_http(&l1).await?;
    let visibility = collect_visibility();
    let tools = collect_tool_execution().await?;
    let mcp = collect_mcp_protocol().await?;
    let coverage = coverage(&http, &tools);
    Ok(json!({
        "schema_version":1,
        "source":{"l1":l1_path.file_name().and_then(|s|s.to_str()).unwrap_or("opencrab-l1.json"),"scenario_catalog":scenario_path.file_name().and_then(|s|s.to_str()).unwrap_or("scenarios.json")},
        "normalization":[
            "content-length response header omitted",
            "timestamp-valued *_at fields and timestamp-valued date_from/date_to fields replaced with <timestamp>",
            "numeric duration_ms/latency_ms replaced with <duration>",
            "UUID strings and subtask-UUID strings replaced with <uuid> markers",
            "implementation-generated unit-*/core-* identifiers replaced with field-specific markers; fixed fixture identifiers remain literal"
        ],
        "scenario_catalog":scenarios,
        "http":http,
        "tool_visibility":visibility,
        "tool_execution":tools,
        "mcp_protocol":mcp,
        "coverage":coverage
    }))
}
