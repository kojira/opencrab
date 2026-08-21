//! L1 baseline collector internals. This module is compiled only by the
//! `baseline-l1` feature and is not part of the production server binary.

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use opencrab_core::engine::ActionExecutor;
use opencrab_gateway::{
    DispatchMode, GatewayActionDef, GatewayActions, SubEngineAccess, ToolClass, ToolSharing,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::{create_router, system_actions::SystemGatewayActions, test_app_state};

fn class_json(class: ToolClass) -> Value {
    let dispatch = match class.dispatch {
        DispatchMode::Inline => "inline",
        DispatchMode::Dispatchable => "dispatchable",
    };
    let sub_engine = match class.sub_engine {
        SubEngineAccess::Allowed => "allowed",
        SubEngineAccess::Blocked => "blocked",
        SubEngineAccess::NotExposed => "not_exposed",
    };
    let sharing = match class.sharing {
        ToolSharing::ConversationBound => "conversation_bound",
        ToolSharing::AgentBound => "agent_bound",
    };
    json!({
        "dispatch": dispatch,
        "sub_engine": sub_engine,
        "sharing": sharing,
    })
}

fn gateway_definition_json(def: GatewayActionDef, origin: &str) -> Value {
    let policy = opencrab_actions::tool_policy(&def.name);
    json!({
        "name": def.name,
        "description": def.description,
        "input_schema": def.parameters,
        "classification": class_json(def.class),
        "visibility": {
            "owner_only": policy.owner_only,
            "trusted_only": policy.trusted_only,
            "depth_capped": policy.depth_capped,
        },
        "origin": origin,
    })
}

fn gateway_definitions_json(defs: Vec<GatewayActionDef>, origin: &str) -> Vec<Value> {
    let mut values: Vec<_> = defs
        .into_iter()
        .map(|def| gateway_definition_json(def, origin))
        .collect();
    values.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    values
}

fn action_context() -> opencrab_actions::ActionContext {
    let workspace = opencrab_core::workspace::Workspace::from_root(std::env::temp_dir())
        .expect("temporary directory must be a valid baseline workspace");
    opencrab_actions::ActionContext {
        agent_id: "baseline-agent".to_string(),
        agent_name: "Baseline Agent".to_string(),
        session_id: Some("baseline-session".to_string()),
        db: opencrab_db::Db::memory().expect("in-memory baseline DB must initialize"),
        workspace: Arc::new(workspace),
        last_metrics_id: Arc::new(std::sync::Mutex::new(None)),
        model_override: Arc::new(std::sync::Mutex::new(None)),
        current_purpose: Arc::new(std::sync::Mutex::new("baseline".to_string())),
        caller: opencrab_actions::CallerIdentity::Owner,
        runtime_info: Arc::new(std::sync::Mutex::new(opencrab_actions::RuntimeInfo {
            default_model: "baseline:model".to_string(),
            active_model: None,
            available_providers: vec![],
            gateway: "baseline".to_string(),
        })),
    }
}

/// Collect tool definitions by invoking the same definition constructors used
/// for an agent run. No external service is contacted.
pub fn collect_tools() -> Value {
    let mut dispatcher = opencrab_actions::ActionDispatcher::new();
    let shell_config = opencrab_actions::ShellToolConfig {
        allowed_commands: vec!["baseline-command".to_string()],
        ..Default::default()
    };
    opencrab_actions::register_tools_from_config(
        &opencrab_actions::ToolsConfig {
            enabled: true,
            shell: Some(shell_config),
        },
        &mut dispatcher,
    );

    let core = opencrab_actions::BridgedExecutor::new(dispatcher, action_context());
    let inline = core.inline_tool_names();
    let mut core_defs: Vec<Value> = core
        .list_tools()
        .into_iter()
        .map(|def| {
            let policy = opencrab_actions::tool_policy(&def.name);
            let is_configured_shell = def.name == "execute_shell";
            // BridgedExecutor synthesizes the full class only for actions in
            // CORE_INLINE_ACTIONS / CORE_DISPATCHABLE_ACTIONS. execute_shell
            // is config-registered, so only its observed dispatch decision is
            // available; do not fill the other fields with plausible defaults.
            let classification = if is_configured_shell {
                json!({
                    "dispatch": if inline.contains(&def.name) { "inline" } else { "dispatchable" },
                    "sub_engine": Value::Null,
                    "sharing": Value::Null,
                    "collection_status": "partial",
                })
            } else {
                class_json(ToolClass {
                    dispatch: if inline.contains(&def.name) {
                        DispatchMode::Inline
                    } else {
                        DispatchMode::Dispatchable
                    },
                    sub_engine: SubEngineAccess::NotExposed,
                    sharing: ToolSharing::AgentBound,
                })
            };
            json!({
                "name": def.name,
                "description": def.description,
                "input_schema": def.parameters,
                "classification": classification,
                "visibility": {
                    "owner_only": policy.owner_only,
                    "trusted_only": policy.trusted_only,
                    "depth_capped": policy.depth_capped,
                },
                "origin": if is_configured_shell { "configured_action" } else { "core_action" },
                "activation": if is_configured_shell {
                    Value::String("tools.enabled && tools.shell.present && tools.shell.enabled".to_string())
                } else {
                    Value::String("always".to_string())
                },
            })
        })
        .collect();
    core_defs.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    let system = SystemGatewayActions::new(test_app_state(), None, None, None);
    let system_defs = gateway_definitions_json(system.definitions(), "effective_system_gateway");

    #[cfg(feature = "discord")]
    let discord_defs = {
        let inner: Arc<dyn GatewayActions> =
            Arc::new(opencrab_discord::DiscordGatewayActions::from_token(
                "baseline-not-a-credential",
                opencrab_db::Db::memory().expect("in-memory Discord baseline DB must initialize"),
                std::env::temp_dir().to_string_lossy().to_string(),
                None,
            ));
        let effective = SystemGatewayActions::new(test_app_state(), Some(inner), None, None);
        gateway_definitions_json(effective.definitions(), "effective_discord_turn")
    };
    #[cfg(not(feature = "discord"))]
    let discord_defs: Vec<Value> = vec![];

    #[cfg(feature = "nostr")]
    let nostr_defs = {
        let inner: Arc<dyn GatewayActions> = Arc::new(opencrab_nostr::NostrGatewayActions::new(
            opencrab_nostr::NostaroCli::new(),
        ));
        let effective = SystemGatewayActions::new(test_app_state(), Some(inner), None, None);
        gateway_definitions_json(effective.definitions(), "effective_nostr_turn")
    };
    #[cfg(not(feature = "nostr"))]
    let nostr_defs: Vec<Value> = vec![];

    json!({
        "core_and_configured": core_defs,
        "effective_gateway_profiles": {
            "without_transport_surface": system_defs,
            "discord_turn": discord_defs,
            "nostr_turn": nostr_defs,
        },
        "uncollected": [
            {
                "kind": "mcp_tools",
                "reason": "Definitions depend on operator-configured external MCP servers and are not L1."
            },
            {
                "kind": "situational_visible_tool_lists",
                "reason": "Caller, depth, allowlist, running gateway, and MCP connection combinations are explicitly outside this L1 scope."
            },
            {
                "kind": "execute_shell_sub_engine_and_sharing_class",
                "reason": "execute_shell is config-registered and has no ToolClass entry in BridgedExecutor; dispatch is observed from inline_tool_names, and the unavailable fields are null rather than inferred."
            }
        ]
    })
}

struct Probe {
    name: &'static str,
    method: &'static str,
    uri: &'static str,
    content_type: Option<&'static str>,
    body: &'static [u8],
}

/// Capture byte-exact responses for deliberately fixed, credential-free inputs.
pub async fn collect_responses() -> Result<Value, String> {
    let probes = [
        Probe {
            name: "health",
            method: "GET",
            uri: "/health",
            content_type: None,
            body: b"",
        },
        Probe {
            name: "api_health",
            method: "GET",
            uri: "/api/health",
            content_type: None,
            body: b"",
        },
        Probe {
            name: "agents_empty",
            method: "GET",
            uri: "/api/agents",
            content_type: None,
            body: b"",
        },
        Probe {
            name: "missing_route",
            method: "GET",
            uri: "/__baseline_l1_missing__",
            content_type: None,
            body: b"",
        },
        Probe {
            name: "method_not_allowed",
            method: "POST",
            uri: "/health",
            content_type: None,
            body: b"",
        },
        Probe {
            name: "malformed_agent_json",
            method: "POST",
            uri: "/api/agents",
            content_type: Some("application/json"),
            body: b"{",
        },
        Probe {
            name: "missing_agent",
            method: "GET",
            uri: "/api/agents/baseline-missing",
            content_type: None,
            body: b"",
        },
    ];

    let mut captured = Vec::new();
    for probe in probes {
        // A fresh in-memory application makes every probe independent of order.
        let app = create_router(test_app_state());
        let mut builder = Request::builder().method(probe.method).uri(probe.uri);
        if let Some(content_type) = probe.content_type {
            builder = builder.header("content-type", content_type);
        }
        let request = builder
            .body(Body::from(probe.body.to_vec()))
            .map_err(|e| format!("{}: request build failed: {e}", probe.name))?;
        let response = app
            .oneshot(request)
            .await
            .map_err(|e| format!("{}: request failed: {e}", probe.name))?;
        let status = response.status().as_u16();
        let mut headers: Vec<Value> = response
            .headers()
            .iter()
            .map(|(name, value)| {
                value
                    .to_str()
                    .map(|v| json!({"name": name.as_str(), "value": v}))
                    .map_err(|e| format!("{}: non-UTF-8 header {}: {e}", probe.name, name))
            })
            .collect::<Result<_, _>>()?;
        headers.sort_by(|a, b| {
            (a["name"].as_str(), a["value"].as_str())
                .cmp(&(b["name"].as_str(), b["value"].as_str()))
        });
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .map_err(|e| format!("{}: response body failed: {e}", probe.name))?;
        let body = std::str::from_utf8(&body)
            .map_err(|e| format!("{}: response body is not UTF-8: {e}", probe.name))?;
        captured.push(json!({
            "name": probe.name,
            "request": {
                "method": probe.method,
                "uri": probe.uri,
                "headers": probe.content_type.map(|v| json!({"content-type": v})).unwrap_or_else(|| json!({})),
                "body_utf8": std::str::from_utf8(probe.body).map_err(|e| format!("{}: request body is not UTF-8: {e}", probe.name))?,
            },
            "response": {
                "status": status,
                "headers": headers,
                "body_utf8": body,
            }
        }));
    }
    Ok(json!({
        "probes": captured,
        "uncollected": [{
            "kind": "all_other_response_scenarios",
            "reason": "Only fixed, credential-free inputs are L1; branch, authorization, and situation coverage is explicitly outside scope."
        }]
    }))
}
