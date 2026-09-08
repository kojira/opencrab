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
use opencrab_core::engine::{ActionExecutor, FunctionDefinition};
use opencrab_gateway::GatewayActions;
use serde::Deserialize;
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::{create_router, process, test_app_state, AppState};

mod catalog;
mod fixture;
mod http_capture;
mod mcp_coverage;
mod tool_execution;
mod tool_setup;

use catalog::*;
use fixture::*;
use http_capture::*;
pub use mcp_coverage::capture;
use tool_execution::*;
use tool_setup::*;

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
        let live_tools: BTreeSet<_> = [
            ToolTransportProfile::WithoutTransport,
            ToolTransportProfile::Discord,
            ToolTransportProfile::Nostr,
        ]
        .into_iter()
        .flat_map(|transport| {
            build_executor_with_state(
                opencrab_actions::CallerIdentity::Owner,
                0,
                true,
                None,
                "default",
                transport,
            )
            .0
            .effective_tool_definitions()
        })
        .map(|definition| definition.definition.name)
        .collect();
        let selected_tools: BTreeSet<_> = catalog
            .tool_execution
            .success_arguments
            .keys()
            .chain(catalog.tool_execution.success_uncollected_l3.keys())
            .cloned()
            .collect();
        assert_eq!(live_tools, selected_tools);
        // Discord is V3-only: transport-owned local action tools are no longer part of the
        // server executor. External capabilities are declared by the connected gateway.
        assert_eq!(
            live_tools
                .iter()
                .filter(|name| {
                    name.starts_with("discord_")
                        || matches!(
                            name.as_str(),
                            "ensure_subtask_webhook"
                                | "ensure_webhook"
                                | "join_voice_channel"
                                | "leave_voice_channel"
                                | "send_ui"
                        )
                })
                .count(),
            0
        );
        // DI フェーズ1: 投稿・操作系の組み込み Nostr ツールは撤去した（能力宣言 DI へ移行・
        // 普通の投稿は say）。組み込み tool surface には出ない（DI operation は runtime 宣言で
        // 静的 surface に現れない）。
        assert_eq!(
            live_tools
                .iter()
                .filter(|name| matches!(
                    name.as_str(),
                    "nostr_post" | "nostr_reply" | "nostr_upload" | "nostr_zap" | "nostr_run"
                ))
                .count(),
            0
        );
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
            json!(["discord", "nostr"])
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
