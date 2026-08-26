# api

HTTP 読口。載せ替え工程 5-b の tool_logs は `llm_logs` と同型。

| 経路 | 形 |
|---|---|
| `GET /api/agents/{id}/tool-logs?limit=` | 配列 JSON（既定 limit=20）。stats は無い |
| `GET /api/sessions` | 配列 JSON。`limit=100` と opaque `before`（直前ページ最後の id）。physical `extgate-*` は出さず logical 1 件 |
| `POST /api/agents/{agent_id}/web-conversations` | `{}` または `{name}`。server 採番。201 ready / 202 provisioning。Bearer なし |
| `GET /api/sessions/{id}` | logical ID。open web binding があれば physical 状態 + `gateway_bound` + `web_binding_state`。不在は 404 |
| `GET /api/sessions/{id}/logs` | mapping があれば physical のみ。`limit=100` と `before`（id） |
| `POST /api/sessions/{id}/owner` | 404。会話 POST は web-gateway |
| `POST /api/sessions/{id}/messages` | 404 |
| `POST /api/sessions/{id}/mentor` | 404 |
| `POST /api/agents/{id}/web/send` | 404 |
| `GET /api/agents/{id}/web/stream` | 404 |

書き込みは `BridgedExecutor`。この層の会話 ingress は置かない。
