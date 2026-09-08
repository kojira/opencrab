use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 49,
    description:
        "標準モデルの必須 budget 値を seed/backfill し fresh/既存 DB の起動を可能にする（#969）",
    // #969: startup は既定モデルと全 agent の実効モデルについて context_window と
    // max_output_tokens を fail-loud で要求する。一方、fresh schema は model_pricing を
    // seed せず、v42 も後から標準在庫へ加わったモデルを NULL のまま残していた。
    //
    // 標準 config の既定 `codex:gpt-5.6` は行自体を exact key で追加する。Codex は
    // subscription 経路なので価格は 0 とし、provider が公称する 1,050,000 context と、
    // OpenCrab 標準 profile の 32,000 output request cap/reserve を登録する。この 32,000 は
    // provider の公称最大値を推測するものではなく、#969 で標準在庫に採用した運用上の上限。
    // したがって「公称値が無いため NULL」とした v42 時点の暫定判断を明示的に更新する。
    //
    // 既存行は既知 model/provider 組の NULL 値だけ補完する。既定 codex 行については
    // INSERT OR IGNORE だけでは既存 row の NULL context を直せないため、context も exact key
    // で補完する。価格、operator が設定済みの非 NULL 値、未知モデルには触れない。
    // INSERT OR IGNORE と IS NULL 条件により再適用しても値を上書きしない。
    up: |conn| {
        conn.execute_batch(
            "INSERT OR IGNORE INTO model_pricing (
                 provider, model, input_price_per_1m, output_price_per_1m,
                 context_window, max_output_tokens, updated_at
             ) VALUES (
                 'codex', 'gpt-5.6', 0.0, 0.0, 1050000, 32000, datetime('now')
             );
             UPDATE model_pricing
                SET context_window = 1050000,
                    updated_at = datetime('now')
              WHERE provider = 'codex'
                AND model = 'gpt-5.6'
                AND context_window IS NULL;
             UPDATE model_pricing
                SET max_output_tokens = 32000,
                    updated_at = datetime('now')
              WHERE max_output_tokens IS NULL
                AND (
                    (provider IN ('codex', 'chatgpt')
                     AND model IN ('gpt-5.6', 'gpt-5.6-sol', 'gpt-5.6-terra', 'gpt-5.6-luna'))
                    OR (provider = 'cursor' AND model = 'cursor-grok-4.6-high')
                );",
        )
    },
}];
