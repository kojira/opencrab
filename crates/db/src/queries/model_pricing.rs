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
    /// v58: cached input read price per 1M tokens. None = base input price applies.
    #[serde(default)]
    pub cached_input_price_per_1m: Option<f64>,
    /// v58: cache write price per 1M tokens. None = base input price applies.
    #[serde(default)]
    pub cache_write_price_per_1m: Option<f64>,
}

/// Token counts of one LLM call, split the way providers bill them.
#[derive(Debug, Clone, Copy, Default)]
pub struct BilledTokens {
    /// Provider-reported prompt count.
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// True when `input_tokens` already contains the cache reads and writes (OpenAI Responses:
    /// `input_tokens_details.cached_tokens`; hermit). False when they are reported separately
    /// from the uncached input (Anthropic-style `cache_read_input_tokens`).
    pub cache_included_in_input: bool,
}

impl ModelPricingRow {
    /// "What this call would cost on the pay-as-you-go API", in USD.
    ///
    /// Cache reads/writes use their own rate when set, else the base input rate.
    pub fn cost_usd(&self, t: BilledTokens) -> f64 {
        let read_rate = self
            .cached_input_price_per_1m
            .unwrap_or(self.input_price_per_1m);
        let write_rate = self
            .cache_write_price_per_1m
            .unwrap_or(self.input_price_per_1m);
        let base_input = if t.cache_included_in_input {
            t.input_tokens - t.cache_read_tokens - t.cache_write_tokens
        } else {
            t.input_tokens
        };
        (base_input.max(0) as f64 * self.input_price_per_1m
            + t.cache_read_tokens.max(0) as f64 * read_rate
            + t.cache_write_tokens.max(0) as f64 * write_rate
            + t.output_tokens.max(0) as f64 * self.output_price_per_1m)
            / 1_000_000.0
    }
}

pub fn upsert_model_pricing(conn: &Connection, pricing: &ModelPricingRow) -> Result<()> {
    conn.execute(
        "INSERT INTO model_pricing (provider, model, input_price_per_1m, output_price_per_1m, context_window, max_output_tokens, updated_at, cached_input_price_per_1m, cache_write_price_per_1m)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(provider, model) DO UPDATE SET
            input_price_per_1m = excluded.input_price_per_1m,
            output_price_per_1m = excluded.output_price_per_1m,
            context_window = excluded.context_window,
            max_output_tokens = excluded.max_output_tokens,
            updated_at = excluded.updated_at,
            cached_input_price_per_1m = excluded.cached_input_price_per_1m,
            cache_write_price_per_1m = excluded.cache_write_price_per_1m",
        params![
            pricing.provider,
            pricing.model,
            pricing.input_price_per_1m,
            pricing.output_price_per_1m,
            pricing.context_window,
            pricing.max_output_tokens,
            Utc::now().to_rfc3339(),
            pricing.cached_input_price_per_1m,
            pricing.cache_write_price_per_1m,
        ],
    )?;
    Ok(())
}

pub fn list_model_pricing(conn: &Connection) -> Result<Vec<ModelPricingRow>> {
    let mut stmt = conn.prepare(
        "SELECT provider, model, input_price_per_1m, output_price_per_1m, context_window, max_output_tokens,
                cached_input_price_per_1m, cache_write_price_per_1m
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
                cached_input_price_per_1m: row.get(6)?,
                cache_write_price_per_1m: row.get(7)?,
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
        "SELECT provider, model, input_price_per_1m, output_price_per_1m, context_window, max_output_tokens,
                cached_input_price_per_1m, cache_write_price_per_1m
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
                cached_input_price_per_1m: row.get(6)?,
                cache_write_price_per_1m: row.get(7)?,
            })
        },
    );

    match result {
        Ok(p) => Ok(Some(p)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
