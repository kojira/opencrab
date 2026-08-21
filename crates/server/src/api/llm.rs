use axum::{extract::State, Json};

use crate::AppState;

/// ダッシュボードのモデルセレクタ用: 既定モデルと各プロバイダの利用可能モデル一覧。
pub async fn model_choices(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut choices: Vec<String> = Vec::new();
    let router = state.llm_router.get();
    for pname in router.provider_names() {
        let Some(prov) = router.get_provider(pname) else {
            continue;
        };
        let prov = prov.clone();
        if let Ok(models) = prov.available_models().await {
            for m in models {
                choices.push(format!("{pname}:{}", m.id));
            }
        }
    }
    choices.sort();
    choices.dedup();
    Json(serde_json::json!({
        "default_model": state.default_model,
        "choices": choices,
    }))
}
