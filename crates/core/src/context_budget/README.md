# context_budget（#826-A envelope / #826-B turn governor）

正典はリポジトリ外の `DESIGN-CONTEXT-BUDGET.md` と `context-budget-rulings-v1.md`。このディレクトリは実装境界だけを書く。

## 水位（826-A）

```text
input_high = min(floor(W * 0.50), A)
input_low  = min(floor(W * 0.25), floor(A / 2))
output_reserve = model_pricing.max_output_tokens
mandatory_fixed = system + runtime_context + functions
fixed = mandatory_fixed + injected_memory_index
conversation_high = input_high.saturating_sub(fixed)
conversation_low  = input_low.saturating_sub(fixed)
```

`output_reserve` は入力予算と別軸なので `mandatory_fixed` に足さない。物理窓ガードは `実入力 + output_reserve <= W`。超えたら `context_budget_exhausted`。

較正前の初期値:

| 項目 | 値 | 根拠 |
|---|---|---|
| 高水位比 | 0.50 | 設計の開始値 |
| 低水位比 | 0.25 | 設計の開始値 |
| 絶対上限 A | 80,000 | 85–90K 劣化開始点より安全側 |
| Memory Index cap | 4,000 | 会話余りを流用しない個別上限 |
| functions cap | 24,000 | 実測 18,327–22,344 の頭 |

`W` は `model_pricing.context_window`。無 / NULL / 0 と予約の無 / NULL / 0 は既定へ落とさず、起動時と解決時に fail-loud。chatgpt 305K 特例と 100K 隠れフォールバックは置かない。全 provider が同じ `min(比例水位, A)` を通る。

`fixed >= input_high`、functions 超過、`実入力 + output_reserve > W` は唯一のエラー名 `context_budget_exhausted`。空履歴で続行しない。

入口（REST / sessions / scheduler / process / AgentRuntime）は `resolve_agent_request_envelope` → `apply_line_items` だけを通す。会話組立へ渡すのは `conversation_high` と `conversation_low`（`build_conversation_string_with_waters`）。MI 判定は `apply_line_items` に一本化し、観測行は `from_envelope`。各 request 前（`run_agent_response`）は実 `list_tools` で functions cap と `fixed >= input_high` を再検査する。

## ターン統御（826-B）

圧縮の正時はターン終了直後の**背景**処理。`run_agent_response` は `tokio::spawn` で `finish_turn` を投げ、利用者の待ち時間に乗せない。`TurnGovernor::finish_turn` が派生スナップショット（`conversation_snapshots`: compacted 会話 + `through_log_id`）を行追加する。DB 正本は不変。

- ターン開始: `assemble_from_snapshot`（スナップショット + 水位印より後の差分）と `inspect_turn_start`。開始時 `fit_logs_to_budget` は走らせない。高水位超過のときだけ `compact_start_if_over`（途中超過と同じ `compact_to_low_water`）。
- ターン途中: SkillEngine の各 append で `TokenLedger` 合計だけを更新し、高水位超過のときだけ合成 user 文字列を低水位まで刈る。実行前に各 tool の result cap 合計を予約し、収まらなければ先に刈る。それでも駄目なら副作用 tool を開始せず `context_budget_exhausted`。
- 二水位: `tokens > conversation_high` で発火し、低水位まで落とす。ちょうど high は非発火。
- 車線順: 直近逐語（must_keep の発話を優先） → エコー参照化 → 古い履歴要約。新しい echo が古い逐語を押し出さない。`ExchangeGroup`（assistant said + 対応 tool call/result）は原子的。
- スナップショット: 非発火時も `assembled.text`（snap+差分の全文）を書く。`items` は正本の全ログから取る。persist 後も継続ターンで刈れる。

完了済み `tool_call.arguments` の read 経路は `{ref,digest,bytes}` の有効 JSON。未決着 call は全文。DB の `metadata.tool_calls_json` は変えない。

発火観測: 本番経路は `take_governor_events` に `Inspect` / `CompactFired` を積む。

## Memory Index / functions

- Memory Index は専用 cap と残予算の双方に収まるときだけ全量注入する。収まらなければ部分切り詰めせず丸ごと省略し、件数と token 数を `context_budget_check` に残す。
- functions は縮約しない。登録時と各 request 前の `ensure_functions_within_cap` / `apply_line_items` で上限を見る。

## 計測

`TokenLedger` はアイテム毎に `measure_item_tokens` を 1 回だけ走らせてキャッシュし、総量は加減算する。巨大アイテムの上限判定は既存 `tokens_reach_limit`（2KiB 窓）。append ごとの全文 encode による O(n²) は禁止。

## 観測

`context_budget_check`（tracing target 同名）に entrypoint・費目・水位・before/after・action/reason を出す。

## 必須テスト（mock LLM のみ）

`core_process_e2e.rs`。(a) 高水位超過→低水位まで削減の数値アサート（境界値込み）と SkillEngine 発火。(b) 本番経路（`finish_turn` / `build_conversation_string_with_waters` / SkillEngine append）の発火点 3 態。(c) 46 往復級の長タスクを圧縮しても発端 user 発話と直近 must_keep 5 speech が残る（逐語窓だけで十分）。(d) 連続 `finish_turn` の実圧縮回数でヒステリシス。

## 劣化帯 harness

`60_000..=110_000` を 5,000 刻みで走査する。各点の `prompt` は conversation 費目として envelope に載せる。実 LLM は呼ばない。選ばれる `input_high` が 305,000 にならないことと A が勝つことを固定する。実測較正は 826-C。
