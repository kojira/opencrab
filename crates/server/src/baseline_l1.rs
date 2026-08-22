//! L1 baseline collector internals. This module is compiled only by the
//! `baseline-l1` feature and is not part of the production server binary.

use std::{collections::BTreeSet, sync::Arc};

use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use opencrab_gateway::{DispatchMode, GatewayActions, SubEngineAccess, ToolClass, ToolSharing};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::{create_router, process, test_app_state};

fn class_json(class: Option<ToolClass>) -> Value {
    let Some(class) = class else {
        return json!({"collection_status":"uncollected"});
    };
    json!({
        "dispatch": match class.dispatch { DispatchMode::Inline => "inline", DispatchMode::Dispatchable => "dispatchable" },
        "sub_engine": match class.sub_engine { SubEngineAccess::Allowed => "allowed", SubEngineAccess::Blocked => "blocked", SubEngineAccess::NotExposed => "not_exposed" },
        "sharing": match class.sharing { ToolSharing::ConversationBound => "conversation_bound", ToolSharing::AgentBound => "agent_bound" },
        "collection_status":"observed"
    })
}

fn action_context(state: &crate::AppState) -> Result<opencrab_actions::ActionContext, String> {
    let workspace = opencrab_core::workspace::Workspace::from_root(std::env::temp_dir())
        .map_err(|error| format!("baseline workspace: {error}"))?;
    Ok(opencrab_actions::ActionContext {
        agent_id: "baseline-agent".to_string(),
        agent_name: "Baseline Agent".to_string(),
        session_id: Some("baseline-session".to_string()),
        db: state.db.clone(),
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
    })
}

fn production_executor(
    shell_enabled: bool,
    gateway_actions: Option<Arc<dyn GatewayActions>>,
) -> Result<opencrab_actions::BridgedExecutor, String> {
    let state = test_app_state();
    *state
        .tools_config
        .write()
        .map_err(|error| error.to_string())? = opencrab_actions::ToolsConfig {
        enabled: shell_enabled,
        shell: shell_enabled.then(|| opencrab_actions::ShellToolConfig {
            allowed_commands: vec!["baseline-command".to_string()],
            ..Default::default()
        }),
    };
    let context = action_context(&state)?;
    Ok(process::build_turn_executor(
        &state,
        process::TurnExecutorWiring {
            context,
            depth: 0,
            gateway_actions,
            subtask_registry: Arc::new(dashmap::DashMap::new()),
            completion_sink: None,
            subtask_starts: None,
            reply_target: None,
            tool_allowlist: None,
        },
        |_| None,
    ))
}

fn definitions_json(
    executor: &opencrab_actions::BridgedExecutor,
    disabled_names: &BTreeSet<String>,
) -> Vec<Value> {
    let mut definitions: Vec<_> = executor
        .effective_tool_definitions()
        .into_iter()
        .map(|tool| {
            let policy = opencrab_actions::tool_policy(&tool.definition.name);
            json!({
                "name": tool.definition.name,
                "description": tool.definition.description,
                "input_schema": tool.definition.parameters,
                "classification": class_json(tool.class),
                "visibility": {
                    "owner_only": policy.owner_only,
                    "trusted_only": policy.trusted_only,
                    "depth_capped": policy.depth_capped,
                },
                "origin": match tool.slot {
                    opencrab_actions::ToolSlot::Dispatcher => "dispatcher",
                    opencrab_actions::ToolSlot::Gateway => "gateway",
                    opencrab_actions::ToolSlot::Mcp => "mcp",
                },
                "activation": if disabled_names.contains(&tool.definition.name) { "always" } else { "observed_only_when_tools_enabled" },
            })
        })
        .collect();
    definitions.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    definitions
}

/// Production executor builder の実出力だけを L1 tool metadata として保存する。
pub fn collect_tools() -> Result<Value, String> {
    let disabled = production_executor(false, None)?;
    let disabled_names: BTreeSet<_> = disabled
        .effective_tool_definitions()
        .into_iter()
        .map(|tool| tool.definition.name)
        .collect();
    let without_transport = production_executor(true, None)?;

    #[cfg(feature = "discord")]
    let discord = {
        let gateway: Arc<dyn GatewayActions> =
            Arc::new(opencrab_discord::DiscordGatewayActions::from_token(
                "baseline-not-a-credential",
                opencrab_db::Db::memory().map_err(|error| error.to_string())?,
                std::env::temp_dir().to_string_lossy().to_string(),
                None,
            ));
        definitions_json(&production_executor(true, Some(gateway))?, &disabled_names)
    };
    #[cfg(not(feature = "discord"))]
    let discord: Vec<Value> = Vec::new();

    #[cfg(feature = "nostr")]
    let nostr = {
        let gateway: Arc<dyn GatewayActions> = Arc::new(opencrab_nostr::NostrGatewayActions::new(
            opencrab_nostr::NostaroCli::new(),
        ));
        definitions_json(&production_executor(true, Some(gateway))?, &disabled_names)
    };
    #[cfg(not(feature = "nostr"))]
    let nostr: Vec<Value> = Vec::new();

    Ok(json!({
        "effective_profiles": {
            "without_transport_surface": definitions_json(&without_transport, &disabled_names),
            "discord_turn": discord,
            "nostr_turn": nostr,
        },
        "uncollected": [{
            "kind":"configured_action_class",
            "reason":"A configured dispatcher action without a production ToolClass index entry is emitted with classification.collection_status=uncollected."
        }]
    }))
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
        let app = create_router(test_app_state());
        let mut builder = Request::builder().method(probe.method).uri(probe.uri);
        if let Some(content_type) = probe.content_type {
            builder = builder.header("content-type", content_type);
        }
        let request = builder
            .body(Body::from(probe.body.to_vec()))
            .map_err(|error| format!("{}: request build failed: {error}", probe.name))?;
        let response = app
            .oneshot(request)
            .await
            .map_err(|error| format!("{}: request failed: {error}", probe.name))?;
        let status = response.status().as_u16();
        let mut headers: Vec<Value> = response
            .headers()
            .iter()
            .map(|(name, value)| {
                value
                    .to_str()
                    .map(|value| json!({"name": name.as_str(), "value": value}))
                    .map_err(|error| format!("{}: non-UTF-8 header {}: {error}", probe.name, name))
            })
            .collect::<Result<_, _>>()?;
        headers.sort_by(|left, right| {
            (left["name"].as_str(), left["value"].as_str())
                .cmp(&(right["name"].as_str(), right["value"].as_str()))
        });
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("{}: response body failed: {error}", probe.name))?;
        let body = std::str::from_utf8(&body)
            .map_err(|error| format!("{}: response body is not UTF-8: {error}", probe.name))?;
        captured.push(json!({
            "name": probe.name,
            "request": {
                "method": probe.method,
                "uri": probe.uri,
                "headers": probe.content_type.map(|value| json!({"content-type": value})).unwrap_or_else(|| json!({})),
                "body_utf8": std::str::from_utf8(probe.body).map_err(|error| format!("{}: request body is not UTF-8: {error}", probe.name))?,
            },
            "response": { "status": status, "headers": headers, "body_utf8": body }
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
