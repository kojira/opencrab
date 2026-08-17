use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Model Pricing
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricingRow {
    pub provider: String,
    pub model: String,
    pub input_price_per_1m: f64,
    pub output_price_per_1m: f64,
    pub context_window: Option<i32>,
    /// #676: そのモデルの出力トークン上限（実能力値）。エンジンが各リクエストの
    /// max_tokens に使う。NULL / 0 以下は「未登録」扱いで、使用時に fail loud で止まる。
    pub max_output_tokens: Option<i32>,
}

pub fn upsert_model_pricing(conn: &Connection, pricing: &ModelPricingRow) -> Result<()> {
    conn.execute(
        "INSERT INTO model_pricing (provider, model, input_price_per_1m, output_price_per_1m, context_window, max_output_tokens, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(provider, model) DO UPDATE SET
            input_price_per_1m = excluded.input_price_per_1m,
            output_price_per_1m = excluded.output_price_per_1m,
            context_window = excluded.context_window,
            max_output_tokens = excluded.max_output_tokens,
            updated_at = excluded.updated_at",
        params![
            pricing.provider,
            pricing.model,
            pricing.input_price_per_1m,
            pricing.output_price_per_1m,
            pricing.context_window,
            pricing.max_output_tokens,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn list_model_pricing(conn: &Connection) -> Result<Vec<ModelPricingRow>> {
    let mut stmt = conn.prepare(
        "SELECT provider, model, input_price_per_1m, output_price_per_1m, context_window, max_output_tokens
         FROM model_pricing ORDER BY provider, model",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ModelPricingRow {
                provider: row.get(0)?,
                model: row.get(1)?,
                input_price_per_1m: row.get(2)?,
                output_price_per_1m: row.get(3)?,
                context_window: row.get(4)?,
                max_output_tokens: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_model_pricing(
    conn: &Connection,
    provider: &str,
    model: &str,
) -> Result<Option<ModelPricingRow>> {
    let result = conn.query_row(
        "SELECT provider, model, input_price_per_1m, output_price_per_1m, context_window, max_output_tokens
         FROM model_pricing WHERE provider = ?1 AND model = ?2",
        params![provider, model],
        |row| {
            Ok(ModelPricingRow {
                provider: row.get(0)?,
                model: row.get(1)?,
                input_price_per_1m: row.get(2)?,
                output_price_per_1m: row.get(3)?,
                context_window: row.get(4)?,
                max_output_tokens: row.get(5)?,
            })
        },
    );

    match result {
        Ok(p) => Ok(Some(p)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
