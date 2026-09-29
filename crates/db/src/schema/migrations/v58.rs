//! v58: prompt-cache unit prices on `model_pricing`.
//!
//! The usage cost ("what this would cost on the pay-as-you-go API") is computed from
//! `model_pricing` instead of a hard-coded table. Cached input reads and cache writes are
//! billed at their own rates, so both get a nullable per-1M price. NULL means the model has
//! no separate rate (the base input price applies).

use super::super::helpers::{column_exists, table_exists};
use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 58,
    description: "add cached input and cache write prices to model_pricing",
    up: |conn| {
        if !table_exists(conn, "model_pricing")? {
            return Ok(());
        }
        if !column_exists(conn, "model_pricing", "cached_input_price_per_1m")? {
            conn.execute_batch(
                "ALTER TABLE model_pricing ADD COLUMN cached_input_price_per_1m REAL;",
            )?;
        }
        if !column_exists(conn, "model_pricing", "cache_write_price_per_1m")? {
            conn.execute_batch(
                "ALTER TABLE model_pricing ADD COLUMN cache_write_price_per_1m REAL;",
            )?;
        }
        Ok(())
    },
}];
