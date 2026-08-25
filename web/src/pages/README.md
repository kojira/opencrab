# pages

エージェント配下の画面。ログ系は LLM ログと同型の一覧。

| 画面 | 経路 | 読口 |
|---|---|---|
| `AgentLlmLogs` | `/agents/:id/llm-logs` | `GET /api/agents/{id}/llm-logs` |
| `AgentToolLogs` | `/agents/:id/tool-logs` | `GET /api/agents/{id}/tool-logs?limit=`（stats 無し） |
