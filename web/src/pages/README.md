# pages

エージェント配下の画面。ログ系は LLM ログと同型の一覧。

| 画面 | 経路 | 読口 |
|---|---|---|
| `AgentLlmLogs` | `/agents/:id/llm-logs` | `GET /api/agents/{id}/llm-logs` |
| `AgentToolLogs` | `/agents/:id/tool-logs` | `GET /api/agents/{id}/tool-logs?limit=`（stats 無し） |
| `Sessions` | `/sessions` | `GET /api/sessions?limit=100&before=`。状態は idle/loading/loaded-empty/loaded/error。読み込んだ頁は切らない。agent filter は `agent_ids` + 同一 `NewConversationButton` |
| `AgentSessions` | agent 配下 | 同上の `limit`/`before`。`agent_ids` で現在の agent を filter。client filter だけで 101 件目を落とさない。同一 `NewConversationButton` |
| `SessionDetail` | `/sessions/:id` | 読はcore。`GET /api/web-conversations/{session_id}`をWeb gatewayへ直接送り、ownershipとbinding状態を確認する。Web会話だけcomposerとevents SSEを使い、非Webはowner指示を使う。ready以外はcomposer無効 + 最大60秒の状態poll。読み込んだlog頁は切らず、SSE message/activityとpending追加で下端へ追従する |

実ブラウザ E2E（§7.4a / §7.4a-r1・jsdom 代替不可）は `web/e2e/`。非 loopback 平文 origin で `isSecureContext===false` を踏み、物理 ID で開いて送信し、溢れリストで最下部到達（1px 台）と上スクロール中の非追従を見る。
