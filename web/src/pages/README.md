# pages

エージェント配下の画面。ログ系は LLM ログと同型の一覧。

| 画面 | 経路 | 読口 |
|---|---|---|
| `AgentLlmLogs` | `/agents/:id/llm-logs` | `GET /api/agents/{id}/llm-logs` |
| `AgentToolLogs` | `/agents/:id/tool-logs` | `GET /api/agents/{id}/tool-logs?limit=`（stats 無し） |
| `Sessions` | `/sessions` | `GET /api/sessions?limit=100&before=`。状態は idle/loading/loaded-empty/loaded/error。読み込んだ頁は切らない |
| `AgentSessions` | agent 配下 | 同上の `limit`/`before`。client filter だけで 101 件目を落とさない |
| `SessionDetail` | `/sessions/:id` | 読は core。送信は `POST /api/web-conversations/{id}/messages`、live は `GET .../events`。SSE 切断と再読込失敗は error + retry。wire 失敗は `gate_error`。binding 未敷設は入力欄なし。読み込んだ log 頁は切らない |
