# queries

ドメイン別の行型と SQL。`mod.rs` から再輸出する。

## gate_session（D-1099）

gateway の会話のキーを core の会話に解決する（`gate_session.rs`）。記録は `gate_bindings` の行で、行 id は core の中だけで使う。前もって登録するものは無い。

| 関数 | 契約 |
|---|---|
| `resolve_gate_session_in_tx` | `(instance, key)` の記録を探し、無ければ作る（呼び出し側のトランザクション内）。新しい記録はキーそのものを session id にし、session 行と参加者が無ければ作る。`title` は新しい会話の題名（省略時はキー） |
| `find_gate_session` | 書き込まずに `(instance, key)` の記録を返す。開いている行を優先し、無ければ最新の行 |
| `gate_session_by_row` | 内部の行 id から記録を返す（frame から始まった処理の続き用） |
| `gate_session_for_session_id` | core の会話につながる記録を返す（定期実行など、frame 無しで始まるターン用） |

## sessions

| 関数 | 契約 |
|---|---|
| `effective_agent_ids` | opaque session IDをそのまま使い、`agent_sessions`をjoinした実効参加者を返す |
| `list_sessions_page` | 全sessionを`updated_at DESC, id DESC`で返す。`limit`とopaqueな`before`を受ける |
| `list_sessions` | テスト専用の全件一覧 |

## tool_logs（載せ替え工程 5-b）

ツール 1 実行 = 1 行。書くのは core（`BridgedExecutor`）。ゲートは書かない。

| 関数 | 契約 |
|---|---|
| `insert_tool_log` | `ToolLogWrite` を受ける。`outcome` は `done\|failed\|refused\|deadline\|stopped`。未知値は拒否（既定へ落とさない） |
| `list_tool_logs` | `agent_id` + `limit`。新しい順。`llm_logs` と同型の読口 |

`memory_sessions` / `llm_logs.tool_calls` は触らない。表定義は [schema/README.md](../schema/README.md)。

## session_watches

表は v63 で削除した（`schema/migrations/v63.rs`）。Nostr の購読は Nostr gateway が持つ。
