use super::Migration;
use crate::schema::table_exists;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 52,
    description: "seed ChatGPT subscription budgets for GPT-6 Sol and Luna",
    up: |conn| {
        if !table_exists(conn, "model_pricing")? {
            return Ok(());
        }
        conn.execute_batch(
            "INSERT OR IGNORE INTO model_pricing (
                 provider, model, input_price_per_1m, output_price_per_1m,
                 context_window, max_output_tokens, updated_at
             ) VALUES
                 ('chatgpt', 'gpt-6-sol', 0.0, 0.0, 400000, 32000, datetime('now')),
                 ('chatgpt', 'gpt-6-luna', 0.0, 0.0, 400000, 32000, datetime('now'));
             UPDATE model_pricing
                SET context_window = 400000,
                    updated_at = datetime('now')
              WHERE provider = 'chatgpt'
                AND model IN ('gpt-6-sol', 'gpt-6-luna')
                AND context_window IS NULL;
             UPDATE model_pricing
                SET max_output_tokens = 32000,
                    updated_at = datetime('now')
              WHERE provider = 'chatgpt'
                AND model IN ('gpt-6-sol', 'gpt-6-luna')
                AND max_output_tokens IS NULL;",
        )
    },
}];
