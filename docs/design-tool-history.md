# 設計書: tool_call履歴管理（最終方針）

## 1. 問題の概要

`spawn_subtask`等の非同期ツール実行時、エージェントの初回発話（例: 「調べてみる。」）がDBに記録されない。サブタスク完了後の`process_subtask_completed`で会話履歴を再構築すると初回発話が欠落し、LLMが同じ発話を再生成してDiscordに重複送信される。

---

## 2. 根本原因

**Anthropic Messages APIの制約**: assistantメッセージに`tool_use`ブロックが含まれる場合、後続のuserメッセージに対応する`tool_result`ブロックが必須。

現状、`process_subtask_completed`が再構築する会話履歴には`tool_use`/`tool_result`ペアが含まれない。そのためAnthropicは「前のターンで何のtool_callをしたか」を把握できず、文脈を失った状態でレスポンスを生成してしまう。

具体的な欠落:
- `on_first_response`での初回発話テキストがDBに未記録
- `tool_use`/`tool_result`の構造体がDBに未保存（最終テキストのみ保存）
- `process_subtask_completed`時にtool_use→tool_resultの完全履歴を復元できない

---

## 3. 最終修正方針

### P0（即時対応・workaround）

**方針1**: `on_first_response`テキストをDBに記録する
**変更箇所**: `crates/discord/src/message_loop.rs`
- `on_first_response`コールバック内で`session_logs`にエージェント発話を挿入
- `process_subtask_completed`の`build_conversation_string`が初回発話を取得できるようになる

**方針2**: `tool_calls`がある場合はassistant contentを空にする
**変更箇所**: `crates/core/src/engine/skill_engine.rs`
- `messages.push`時、`tool_calls`が非空なら`content = ""`にする
- Anthropicが「中断された思考の補完」として重複テキストを生成するのを防ぐ

### P1（根本解決・優先）

**方針3**: `tool_use`/`tool_result`をDBに保存し、`process_subtask_completed`時に完全復元する

**変更箇所**:
1. DBスキーマ: `subtask_tool_calls`テーブルを追加（`session_id`, `tool_call_id`, `subtask_id`, `input_json`）
2. `crates/discord/src/gateway_actions/subtask_engine.rs`: spawn時にtool_call_idとsubtask_idの対応をDBに保存
3. `crates/server/src/process.rs`の`process_subtask_completed`: DBからtool_call_idを取得し、messages配列に`tool_use`→`tool_result`ペアを復元してLLMに渡す

---

## 4. 期待動作（修正後）

**非同期ツール（spawn_subtask）の正常フロー**:

```
iteration 1:
  LLM → "調べてみる。" + tool_calls:[spawn_subtask]
  on_first_response → Discord送信 + DB記録 ✓（方針1）
  messages配列 → {role:assistant, content:"", tool_calls:[...]}（方針2）

iteration 2:
  tool_result = {"status":"spawned","subtask_id":"..."}
  LLM → "" or NO_REPLY

サブタスク完了時:
  build_conversation_string → [user: "教えて"][agent: "調べてみる。"] ✓
  （P1適用時）tool_use/tool_resultペアも復元してLLMに渡す
  LLM → "〇〇の結果です。"（重複なし）

Discordメッセージ: 「調べてみる。」→「〇〇の結果です。」（2回、重複なし）
```
