use super::super::*;
use super::support::*;

/// **レスポンス JSON が移設前と同一**（記憶インデックス設定）。
/// `previous` / `current` の入れ子形をリテラルで固定する。
#[tokio::test]
async fn update_memory_index_config_response_json_is_unchanged() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    // 未設定からの更新: previous は既定値。
    let r = actions
        .execute(
            "update_memory_index_config",
            &json!({"batch_size": 10}),
            &agent_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "agent_id": "agent-x",
            "previous": {
                "batch_size": opencrab_db::queries::BATCH_SIZE_DEFAULT,
                "threshold": opencrab_db::queries::THRESHOLD_DEFAULT,
            },
            "current": { "batch_size": 10, "threshold": opencrab_db::queries::THRESHOLD_DEFAULT },
        })
    );

    // 片方だけ指定すると、もう片方は現状維持。
    let r = actions
        .execute(
            "update_memory_index_config",
            &json!({"threshold": 5}),
            &agent_ctx(),
        )
        .await;
    assert!(r.success);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "agent_id": "agent-x",
            "previous": { "batch_size": 10, "threshold": opencrab_db::queries::THRESHOLD_DEFAULT },
            "current": { "batch_size": 10, "threshold": 5 },
        })
    );

    // DB へ永続化されている。
    let conn = state.db.lock().unwrap();
    let cfg = opencrab_db::queries::get_memory_index_config(&conn, "agent-x").unwrap();
    assert_eq!((cfg.batch_size, cfg.threshold), (10, 5));
}

/// 引数が両方欠けているときは移設前と同じ文言で失敗する。
#[tokio::test]
async fn update_memory_index_config_requires_at_least_one_field() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let r = actions
        .execute("update_memory_index_config", &json!({}), &agent_ctx())
        .await;
    assert!(!r.success);
    assert_eq!(
        r.error.as_deref(),
        Some("batch_sizeまたはthresholdの少なくとも1つが必要です")
    );
}
