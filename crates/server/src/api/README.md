# api

HTTP 読口。載せ替え工程 5-b の tool_logs は `llm_logs` と同型。

| 経路 | 形 |
|---|---|
| `GET /api/agents/{id}/tool-logs?limit=` | 配列 JSON（既定 limit=20）。stats は無い |
| `POST /api/sessions/{id}/owner` | `{ id, session_id, responses? }`。判断は `accept_inbound`、記録は `prepare_session_inbound_write`、ターンは `run_session_turn` |

書き込みは `BridgedExecutor`。この層は読まない。owner は例外で、ダッシュボード SessionDetail の指示を core inbound 1 口へ載せる。
