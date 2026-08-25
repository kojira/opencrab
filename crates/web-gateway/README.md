# opencrab-web-gateway

ダッシュボード会話ゲート。載せ替え工程 4-b 以降、**配送専用**。

会話の単位はセッションのまま。`session_id` は `web-{agent_id}-{conversation_id}`。
書式も表も外形 API（`POST .../web/send` / `GET .../web/stream?conversation=`）も変えない。

## 残すもの（配送）

- HTTP 受け（`user_id` 正規化・JSON 応答）
- 受信の正規化（生識別子を `NormalizedInboundEvent` にする）
- SSE（`subscribe` / `publish`）
- core の決定（`DeliveryEffect`）の送信
- `ensure_web_session` / `record_user_message` / `record_agent_reply`（行の形は現行のまま。Discord の `TranscriptSource` には載せない）

## 移した判断（core = `opencrab-actions`）

| 判断 | 元 | 先 |
|---|---|---|
| caller 解決（誰か・権限） | `http.rs` が `WebAgentRunner::resolve_caller` を直呼び | `plan_inbound`（`WebInboundIdentity` 越し。計算本体は runner 実装） |
| ターン起動（文脈・run・NO_REPLY） | `respond.rs` が `build_conversation_string` / `run_agent_response` を直呼び | `run_session_turn` + `delivery_effect` |

ゲートは生識別子（正規化済み `user_id`）だけを core へ渡す。
`AppState` の `InboundIdentity` は Discord 経路固定なので、web は
`WebInboundIdentity` で同じ `plan_inbound` に載せる（別 inbound 口は作らない）。

voice / nostr は触らない。

## 関連

- [opencrab-actions](../actions/README.md)
- [opencrab-discord](../discord/README.md)
