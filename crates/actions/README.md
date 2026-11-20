# opencrab-actions

エージェントが使う道具（アクション）と、ターン実行の集約口。

## アクション

`ActionDispatcher::new()`（`src/dispatcher.rs`）が core のアクションを登録する。一覧は
リポジトリ直下の `README.md` の「Action System」表が正本で、テスト
`readme_action_table_matches_the_dispatcher` が両者の一致を守る。

`execute_shell` は設定駆動（`[tools.shell]`、`src/tools/`）で、`ActionDispatcher::new()` には入らない。

- `src/bridge/` — `BridgedExecutor`（core の `ActionExecutor` 実装）。権限の表（`policy.rs` の
  `OWNER_ONLY_ACTIONS` / `TRUSTED_ONLY_ACTIONS`）、inline / dispatch の分類（`CORE_INLINE_ACTIONS` /
  `CORE_DISPATCHABLE_ACTIONS`）、ツール 1 実行ごとの `tool_logs` 記録（`done` / `failed` / `refused`）。
- `src/model_admin.rs` — `ModelAdministration`（`/model` コマンドと `select_llm` が共有するモデル管理）と
  `resolve_exact_model`（モデル名の解決は 1 か所）。
- `src/llm_selection.rs` — `select_llm`。この会話のモデル（`sessions.model`）を `set_session_model` で
  変え、次のターンから効く（D-1135）。

## セッション inbound（`session_inbound`）

ゲートは正規化した受信（`NormalizedInbound`: 本文・送信者の生識別子・対象 session_id）を渡す。
誰が話してよいかはゲートが決める（D-1099）。配送（送信・描画・typing・webhook）もゲート。
core が返すのは配送 effect。

| 関数 | 役割 |
|---|---|
| `prepare_session_inbound` / `prepare_session_inbound_write` | セッションの確保と受信の記録（ロック前） |
| `start_session_turn` | 受信フック + 会話構築 + run |
| `run_session_turn` | resume / 継続（フックなし） |
| `delivery_effect` | ターン結果 → `Text` / `NoReply` / `Empty` / `Failed` |

## 関連

- gateway の実装は [opencrab-gateways](https://github.com/kojira/opencrab-gateways) と
  [opencrab-bluesky-gateway](https://github.com/kojira/opencrab-bluesky-gateway)
