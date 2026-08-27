# context_budget（#826-A: envelope と観測基盤）

正典はリポジトリ外の `DESIGN-CONTEXT-BUDGET.md`。このディレクトリは 826-A の実装境界だけを書く。826-B（ターン統御・スナップショット・車線・checkpoint）は扱わない。

## 水位

```text
input_high = min(floor(W * 0.50), A)
input_low  = min(floor(W * 0.25), floor(A / 2))
output_reserve = model_pricing.max_output_tokens
mandatory_fixed = system + runtime_context + functions + output_reserve
fixed = mandatory_fixed + injected_memory_index
conversation_high = input_high.saturating_sub(fixed)
conversation_low  = input_low.saturating_sub(fixed)
```

較正前の初期値:

| 項目 | 値 | 根拠 |
|---|---|---|
| 高水位比 | 0.50 | 設計の開始値 |
| 低水位比 | 0.25 | 設計の開始値 |
| 絶対上限 A | 80,000 | 85–90K 劣化開始点より安全側 |
| Memory Index cap | 4,000 | 会話余りを流用しない個別上限 |
| functions cap | 24,000 | 実測 18,327–22,344 の頭 |

`W` は `model_pricing.context_window`。無 / NULL / 0 と予約の無 / NULL / 0 は既定へ落とさず、起動時と解決時に fail-loud。chatgpt 305K 特例と 100K 隠れフォールバックは置かない。全 provider が同じ `min(比例水位, A)` を通る。

`fixed >= input_high` と functions 超過は唯一のエラー名 `context_budget_exhausted`。空履歴で続行しない。

## Memory Index / functions

- Memory Index は専用 cap と残予算の双方に収まるときだけ全量注入する。収まらなければ部分切り詰めせず丸ごと省略し、件数と token 数を `context_budget_check` に残す。
- functions は縮約しない。登録時と各 request 前の `ensure_functions_within_cap` で上限を見る。

## 計測

`TokenLedger` はアイテム毎に `measure_item_tokens` を 1 回だけ走らせてキャッシュし、総量は加減算する。巨大アイテムの上限判定は既存 `tokens_reach_limit`（2KiB 窓）。append ごとの全文 encode による O(n²) は禁止。

## 観測

`context_budget_check`（tracing target 同名）に entrypoint・費目・水位・before/after・action/reason を出す。

## 劣化帯 harness

`60_000..=110_000` を 5,000 刻みで走査する mock harness を置く。実 LLM は呼ばない。選ばれる `input_high` が 305,000 にならないことと A が勝つことを固定する。実測較正は 826-C。
