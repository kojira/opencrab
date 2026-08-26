# pages

エージェント配下の画面。ログ系は LLM ログと同型の一覧。

| 画面 | 経路 | 読口 |
|---|---|---|
| `AgentLlmLogs` | `/agents/:id/llm-logs` | `GET /api/agents/{id}/llm-logs` |
| `AgentToolLogs` | `/agents/:id/tool-logs` | `GET /api/agents/{id}/tool-logs?limit=`（stats 無し） |
| `Sessions` | `/sessions` | `GET /api/sessions?limit=100&before=`。状態は idle/loading/loaded-empty/loaded/error。読み込んだ頁は切らない。agent filter は `agent_ids` + 同一 `NewConversationButton` |
| `AgentSessions` | agent 配下 | 同上の `limit`/`before`。`agent_ids` で現在の agent を filter。client filter だけで 101 件目を落とさない。同一 `NewConversationButton` |
| `SessionDetail` | `/sessions/:id` | 読は core。web 会話（`gateway_bound`＝open web binding の address または physical）だけ composer と `GET .../events`。非 web は読取 + owner 指示（SSE なし）。web の送信は `POST /api/web-conversations/{id}/messages`。SSE 切断と再読込失敗は error + retry。wire 失敗は `gate_error`。`web_binding_state` が ready 以外なら composer 無効 + 「gateway binding 準備中」+ 最大 60 秒 1 秒間隔の GET。読み込んだ log 頁は切らない |
