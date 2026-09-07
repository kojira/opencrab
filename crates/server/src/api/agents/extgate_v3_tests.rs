use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::{create_router, production_route_inventory, test_app_state};

#[tokio::test]
async fn get_agent_absent_is_200_null_existing_has_subject_id() {
    let state = test_app_state();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: "agent-one".into(),
                name: "One".into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "p".into(),
                personality: None,
                instructions: String::new(),
                heartbeat_instructions: String::new(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .unwrap();
    }
    let app = create_router(state.clone());
    let missing = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/agents/no-such")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::OK);
    let missing_body = missing.into_body().collect().await.unwrap().to_bytes();
    let missing_v: serde_json::Value = serde_json::from_slice(&missing_body).unwrap();
    assert!(missing_v.is_null());

    let ok = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/agents/agent-one")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let body = ok.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let sid = v["subject_id"].as_i64().expect("subject_id");
    assert!(sid > 0);
}

#[test]
fn gate_admin_paths_are_exactly_six() {
    let routes = production_route_inventory();
    let gate: Vec<_> = routes
        .iter()
        .filter(|r| r.path.starts_with("/api/gate"))
        .collect();
    let paths: Vec<&str> = gate.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "/api/gate-bindings/{binding_id}",
            "/api/gate-instances/{instance_id}",
            "/api/gate-instances/{instance_id}/revisions",
        ]
    );
    let methods: Vec<Vec<String>> = gate.iter().map(|r| r.methods.clone()).collect();
    assert_eq!(
        methods,
        vec![
            vec!["DELETE".to_string(), "PUT".to_string()],
            vec!["DELETE".to_string(), "GET".to_string(), "PUT".to_string()],
            vec!["POST".to_string()],
        ]
    );
}

#[tokio::test]
async fn health_does_not_require_gate_bearer() {
    let app = create_router(test_app_state());
    let res = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}
