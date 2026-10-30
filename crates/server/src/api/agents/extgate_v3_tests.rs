use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::{create_router, test_app_state};

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
