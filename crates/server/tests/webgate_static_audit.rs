//! §2.2: 禁止 production 参照 0、server から gateway crate 依存 0。

use std::fs;
use std::path::{Path, PathBuf};

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn walk_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("tests") {
                continue;
            }
            walk_rs(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn forbidden_production_references_are_zero() {
    let src = crate_root().join("src");
    let mut files = Vec::new();
    walk_rs(&src, &mut files);
    let patterns = [
        "send_web_message",
        "web_stream",
        "send_owner_instruction",
        "send_mentor_instruction",
        "fn send_message",
        "WebCompletionSink",
        "WebTimedFireSink",
        "WEB_SESSION_PREFIX",
        "opencrab_web_gateway",
        "opencrab-web-gateway",
    ];
    let mut hits = Vec::new();
    for path in &files {
        let text = fs::read_to_string(path).unwrap();
        for pat in patterns {
            if text.contains(pat) {
                hits.push(format!("{}: {pat}", path.display()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "forbidden production references:\n{}",
        hits.join("\n")
    );
}

#[test]
fn server_cargo_has_no_web_gateway_dependency() {
    let toml = fs::read_to_string(crate_root().join("Cargo.toml")).unwrap();
    assert!(
        !toml.contains("opencrab-web-gateway"),
        "server Cargo.toml still depends on opencrab-web-gateway"
    );
    assert!(!toml.contains("web = "), "server still has a web feature");
}

const WITHDRAWN_CORE_CONVERSATION: &[&str] = &[
    "/api/agents/{id}/web/send",
    "/api/agents/{id}/web/stream",
    "/api/sessions/{id}/owner",
    "/api/sessions/{id}/messages",
    "/api/sessions/{id}/mentor",
];

const UI_FORBIDDEN: &[&str] = &[
    "/web/send",
    "/web/stream",
    "/sessions/${id}/owner",
    "/sessions/${id}/mentor",
    "/sessions/${id}/messages",
    "/rooms/",
];

fn walk_ext(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            walk_ext(&path, ext, out);
        } else if path.extension().and_then(|n| n.to_str()) == Some(ext) {
            out.push(path);
        }
    }
}

fn public_gate_admin_route_debt(
    routes: &[opencrab_server::HttpRouteDescriptor],
) -> Vec<(String, Vec<String>)> {
    routes
        .iter()
        .filter(|route| {
            route.path.starts_with("/api/gate-instances")
                || route.path.starts_with("/api/gate-bindings")
        })
        .map(|route| (route.path.clone(), route.methods.clone()))
        .collect()
}

#[test]
fn public_gate_admin_route_debt_matches_s0_burn_down() {
    // V07 / owner S1: this is debt, not an approved public API. S1 expires the
    // entire list by moving all six operations to the protected core UDS.
    let actual = public_gate_admin_route_debt(&opencrab_server::production_route_inventory());
    let expected = vec![
        (
            "/api/gate-bindings/{binding_id}".to_string(),
            vec!["DELETE".to_string(), "PUT".to_string()],
        ),
        (
            "/api/gate-instances/{instance_id}".to_string(),
            vec!["DELETE".to_string(), "GET".to_string(), "PUT".to_string()],
        ),
        (
            "/api/gate-instances/{instance_id}/revisions".to_string(),
            vec!["POST".to_string()],
        ),
    ];
    assert_eq!(
        actual, expected,
        "unclassified public gate-admin route debt"
    );
}

#[test]
fn public_gate_admin_route_detector_rejects_an_added_route() {
    let mut routes = opencrab_server::production_route_inventory();
    let reviewed_debt = public_gate_admin_route_debt(&routes);
    routes.push(opencrab_server::HttpRouteDescriptor {
        path: "/api/gate-instances/{instance_id}/unclassified".to_string(),
        methods: vec!["POST".to_string()],
        activation: "always".to_string(),
        source: "mutation-fixture".to_string(),
    });
    assert_ne!(
        public_gate_admin_route_debt(&routes),
        reviewed_debt,
        "an added public gate-admin route must not match reviewed debt"
    );
}

#[test]
fn route_inventory_has_no_withdrawn_conversation_post() {
    let routes = opencrab_server::production_route_inventory();
    let mut hits = Vec::new();
    for route in &routes {
        if WITHDRAWN_CORE_CONVERSATION.contains(&route.path.as_str())
            || route
                .path
                .split('/')
                .any(|segment| segment == "web-conversations")
        {
            hits.push(format!("{} {}", route.methods.join(","), route.path));
        }
    }
    assert!(
        hits.is_empty(),
        "withdrawn conversation routes still in inventory:\n{}",
        hits.join("\n")
    );
}

#[test]
fn ui_call_sites_have_no_unknown_conversation_post() {
    let web_src = crate_root().join("../../web/src");
    let mut files = Vec::new();
    walk_ext(&web_src, "ts", &mut files);
    walk_ext(&web_src, "tsx", &mut files);
    let mut hits = Vec::new();
    let mut new_post = 0usize;
    for path in &files {
        let text = fs::read_to_string(path).unwrap();
        if text.contains("/api/web-conversations/") && text.contains("POST") {
            new_post += 1;
        }
        if text.contains("sendWebMessage") || text.contains("conversationEventsUrl") {
            new_post += 1;
        }
        for pat in UI_FORBIDDEN {
            if text.contains(pat) {
                hits.push(format!("{}: {pat}", path.display()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "unknown/withdrawn conversation call-site:\n{}",
        hits.join("\n")
    );
    assert!(new_post >= 1, "UI send call-site for new POST is missing");
}

#[test]
fn ui_has_exactly_one_create_web_conversation_client() {
    let web_src = crate_root().join("../../web/src");
    let mut files = Vec::new();
    walk_ext(&web_src, "ts", &mut files);
    walk_ext(&web_src, "tsx", &mut files);
    let mut defs = Vec::new();
    for path in &files {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.contains(".test.") {
            continue;
        }
        let text = fs::read_to_string(path).unwrap();
        if text.contains("export async function createWebConversation") {
            defs.push(path.display().to_string());
        }
    }
    assert_eq!(
        defs,
        vec![web_src.join("api/sessions.ts").display().to_string()],
        "create client must be exactly one production function"
    );
}

#[test]
fn operator_bearer_is_absent_from_gateway_and_browser() {
    let mut files = Vec::new();
    walk_ext(&crate_root().join("../../web/src"), "ts", &mut files);
    walk_ext(&crate_root().join("../../web/src"), "tsx", &mut files);
    walk_rs(&crate_root().join("../web-gateway/src"), &mut files);
    let mut hits = Vec::new();
    for path in &files {
        let text = fs::read_to_string(path).unwrap();
        if text.contains("OPENCRAB_GATE_OPERATOR_TOKEN") || text.contains("Authorization: Bearer") {
            hits.push(path.display().to_string());
        }
    }
    assert!(
        hits.is_empty(),
        "operator Bearer leaked into gateway/browser:\n{}",
        hits.join("\n")
    );
}

#[test]
fn withdrawn_posts_do_not_create_bindings() {
    let sessions = fs::read_to_string(crate_root().join("src/api/sessions.rs")).unwrap();
    assert!(
        !sessions.contains("create_gate_binding_in_tx"),
        "POST /api/sessions must not create a gate binding"
    );
    let mut gw = Vec::new();
    walk_rs(&crate_root().join("../web-gateway/src"), &mut gw);
    let mut hits = Vec::new();
    for path in &gw {
        let text = fs::read_to_string(path).unwrap();
        if text.contains("create_gate_binding_in_tx") || text.contains("INSERT INTO gate_bindings")
        {
            hits.push(path.display().to_string());
        }
    }
    assert!(
        hits.is_empty(),
        "gateway message POST must not create bindings:\n{}",
        hits.join("\n")
    );
}
