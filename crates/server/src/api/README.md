# api

HTTP 読口。載せ替え工程 5-b の tool_logs は `llm_logs` と同型。

| 経路 | 形 |
|---|---|
| `GET /api/agents/{id}/tool-logs?limit=` | 配列 JSON（既定 limit=20）。stats は無い |

書き込みは `BridgedExecutor`。この層は読まない。
