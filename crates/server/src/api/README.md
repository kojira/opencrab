# api

HTTP 読口。載せ替え工程 5-b の tool_logs は `llm_logs` と同型。

| 経路 | 形 |
|---|---|
| `GET /api/agents/{id}/tool-logs?limit=` | 配列 JSON（既定 limit=20）。stats は無い |
| `GET /api/sessions` | 配列 JSON。`limit=100` と opaque `before`（直前ページ最後の id）。各行に `agent_ids`（`agent_sessions` join）。既定ソート `updated_at DESC` |
| `GET /api/sessions/{id}` | opaque IDをそのまま検索する。各行に`agent_ids`。不在は404 |
| `GET /api/sessions/{id}/logs` | opaque session IDをそのまま検索する。`limit=100` と `before`（id） |
| `POST /api/sessions/{id}/owner` | 404。会話 POST は web-gateway |
| `POST /api/sessions/{id}/messages` | 404 |
| `POST /api/sessions/{id}/mentor` | 404 |
| `POST /api/agents/{id}/web/send` | 404 |
| `GET /api/agents/{id}/web/stream` | 404 |

書き込みは `BridgedExecutor`。この層の会話 ingress は置かない。
