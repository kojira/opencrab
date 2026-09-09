use super::*;
// 本体は `is_owner_equivalent()` 経由でしか caller を見ないので、列挙子を組み立てるのは
// テストだけ。本体側の `use` に混ぜると未使用警告になる。
use opencrab_gateway::GatewayCaller;

const AGENT: &str = "agent-x";

fn state_with_agent(model: Option<&str>) -> AppState {
    let state = crate::test_app_state();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: AGENT.to_string(),
                name: "Agent X".to_string(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "X".to_string(),
                personality: None,
                instructions: String::new(),
                heartbeat_instructions: String::new(),
                model: model.map(|s| s.to_string()),
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .unwrap();
    }
    state
}

fn register(state: &AppState, provider: &str, model: &str, window: i32) {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::upsert_model_pricing(
        &conn,
        &opencrab_db::queries::ModelPricingRow {
            provider: provider.to_string(),
            model: model.to_string(),
            input_price_per_1m: 0.0,
            output_price_per_1m: 0.0,
            context_window: Some(window),
            // #676: テストの router は空でプロバイダ能力が既定（送る＝登録必須）に倒れるため、
            // 「完全登録」を表すには max_output_tokens も入れる（context_window だけでは gate を
            // 通らない）。gate の条件分岐そのものは context_budget の単体テストで担保する。
            max_output_tokens: Some(8192),
        },
    )
    .unwrap();
}

fn stored_model(state: &AppState) -> Option<String> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::get_agent(&conn, AGENT)
        .unwrap()
        .unwrap()
        .model
}

async fn configure(
    state: &AppState,
    caller: GatewayCaller,
    args: serde_json::Value,
) -> GatewayActionResult {
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    actions
        .execute(
            "configure_self",
            &args,
            &GatewayCallContext::new(caller, AGENT),
        )
        .await
}

#[tokio::test]
async fn rejects_unregistered_model() {
    let state = state_with_agent(None);
    let r = configure(
        &state,
        GatewayCaller::Owner,
        json!({"model": "testprov:unregistered"}),
    )
    .await;
    assert!(!r.success);
    let e = r.error.unwrap();
    assert!(e.contains("model_pricing"), "{e}");
    assert!(e.contains("/api/llm/model-pricing"), "{e}");
    // 拒否した以上、保存もされない。
    assert_eq!(stored_model(&state), None);
}

#[tokio::test]
async fn accepts_registered_model() {
    let state = state_with_agent(None);
    register(&state, "testprov", "testmodel", 200_000);
    let r = configure(
        &state,
        GatewayCaller::Owner,
        json!({"model": "testprov:testmodel"}),
    )
    .await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(stored_model(&state), Some("testprov:testmodel".to_string()));
}

/// 既存の未登録モデルを載せたまま**別のフィールドだけ**変える操作は通る
/// （gate が「新しく設定するとき」にだけ効いていること）。
#[tokio::test]
async fn other_fields_change_while_unregistered_model_stays() {
    let state = state_with_agent(Some("testprov:legacy"));
    let r = configure(&state, GatewayCaller::Owner, json!({"job_title": "研究員"})).await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(stored_model(&state), Some("testprov:legacy".to_string()));
}

/// owner 限定の扱いは変えていない。gate の追加で権限判定が緩んでいないこと。
#[tokio::test]
async fn non_owner_is_still_rejected() {
    let state = state_with_agent(None);
    register(&state, "testprov", "testmodel", 200_000);
    let r = configure(
        &state,
        GatewayCaller::Agent,
        json!({"model": "testprov:testmodel"}),
    )
    .await;
    assert!(!r.success);
    assert!(r.error.unwrap().contains("owner"));
    assert_eq!(stored_model(&state), None);
}
