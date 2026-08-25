# queries

ドメイン別の行型と SQL。`mod.rs` から再輸出する。

## tool_logs（載せ替え工程 5-b）

ツール 1 実行 = 1 行。書くのは core（`BridgedExecutor`）。ゲートは書かない。

| 関数 | 契約 |
|---|---|
| `insert_tool_log` | `ToolLogWrite` を受ける。`outcome` は `done\|failed\|refused\|deadline\|stopped`。未知値は拒否（既定へ落とさない） |
| `list_tool_logs` | `agent_id` + `limit`。新しい順。`llm_logs` と同型の読口 |

`memory_sessions` / `llm_logs.tool_calls` は触らない。表定義は [schema/README.md](../schema/README.md)。

## session_watches（載せ替え工程 5-a）

セッションに紐づく Nostr 購読。1 セッション N 行。`interval_secs` は必須・正の整数。

| 関数 | 契約 |
|---|---|
| `insert_session_watch` | 1 行追加。`id` を返す。不正な interval / filter は拒否 |
| `get_session_watch` | `id` で 1 行。無ければ `None` |
| `list_session_watches_for_agent` | その agent の接続で実行する watch を id 順 |
| `update_session_watch` | 1 行更新。対象が無ければ `false` |
| `delete_session_watch` | 1 行削除。対象が無ければ `false` |

本番の読口は `list_session_watches_for_agent`（API / runner）。セッション横断の有無判定は置かない。
