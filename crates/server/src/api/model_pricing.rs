//! モデル単価とコンテキスト長の登録 API（#412）。
//!
//! `model_pricing` は文脈予算（`context_window × compaction_ratio`）の唯一の出所
//! だが、`upsert_model_pricing` の呼び出し元がテストしか無く、**行を入れる手段が
//! 存在しなかった**。入れる手段が無いので誰も入れず、空でも既定値で黙って動くので
//! 誰も困らない ——「気づけない壊れ方」がここで固定されていた。
//!
//! ここが投入経路。モデルを設定する側（`PUT`/`PATCH /api/agents/{id}`、
//! `configure_self` ツール、config の `[llm] default_model`）は、この API で登録済みの
//! モデルしか受け付けない。
//!
//! # 登録が必要な spec
//!
//! 「エージェントが使っているモデル」だけでは足りない。**登録すべきは次の和集合**:
//!
//! - `agents.model` に入っている distinct な spec すべて
//! - config の `[llm] default_provider` と `default_model` を `provider:model` の形に
//!   繋いだ spec（`agents.model` が空のエージェントの実効モデルであり、ホットリロードの
//!   検証対象でもある）
//!
//! 後者が前者に含まれるとは限らない。**落とすと `config/*.toml` を触った瞬間の
//! リロードが毎回丸ごと拒否される**（tools だけの変更であっても）。登録後は
//! `GET /api/llm/model-pricing` で件数を確認すること。
//!
//! provider / model は両端の空白を落として保存する。参照側（`process::model_pricing_key`）
//! も同じ正規化を掛けるので、「登録したのに未登録と言われる」は起きない。

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::AppState;

pub async fn list_model_pricing(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let conn = state
        .db
        .lock()
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows = opencrab_db::queries::list_model_pricing(&conn)
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // 文脈予算 = context_window × compaction_ratio。context_window は行ごとの生値
    // だが compaction_ratio は server-global の単一値なので、掛け算に必要なもう片方
    // として同じレスポンスに載せる。これが無いとフロントは実効予算を出せず、
    // 「context_window が小さすぎる」異常に気づけない（#484）。
    Ok(Json(json!({
        "models": rows,
        "compaction_ratio": state.compaction_ratio,
    })))
}

#[derive(Debug, Deserialize)]
pub struct PutModelPricingBody {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub input_price_per_1m: f64,
    #[serde(default)]
    pub output_price_per_1m: f64,
    /// そのモデルの最大コンテキスト長（トークン）。
    /// **必須**。これが登録の目的なので、省略や 0 以下は受け付けない。
    pub context_window: i32,
}

pub async fn put_model_pricing(
    State(state): State<AppState>,
    Json(body): Json<PutModelPricingBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if body.provider.trim().is_empty() || body.model.trim().is_empty() {
        return Err(bad(
            StatusCode::BAD_REQUEST,
            "provider and model are required".to_string(),
        ));
    }
    if body.context_window <= 0 {
        return Err(bad(
            StatusCode::BAD_REQUEST,
            "context_window must be a positive number of tokens".to_string(),
        ));
    }

    let row = opencrab_db::queries::ModelPricingRow {
        provider: body.provider.trim().to_string(),
        model: body.model.trim().to_string(),
        input_price_per_1m: body.input_price_per_1m,
        output_price_per_1m: body.output_price_per_1m,
        context_window: Some(body.context_window),
    };

    let conn = state
        .db
        .lock()
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    opencrab_db::queries::upsert_model_pricing(&conn, &row)
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({
        "saved": true,
        "provider": row.provider,
        "model": row.model,
        "context_window": body.context_window,
    })))
}

fn bad(code: StatusCode, msg: String) -> (StatusCode, Json<serde_json::Value>) {
    (code, Json(json!({ "error": msg })))
}
