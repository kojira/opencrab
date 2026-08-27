# queries

ドメイン別の行型と SQL。`mod.rs` から再輸出する。

## nostr instance 敷設

Nostr の instance/binding 書き込みは `crates/server/src/nostr_provision.rs`。
`create_gate_binding_in_tx` を使い、address=既存 session_id で V3.5 reuse する。
session 不在・membership 不一致は fail-loud。

## gate_binding

| 関数 | 契約 |
|---|---|
| `create_gate_binding_in_tx` | physical session・sole membership・open binding を 1 TX。theme は呼び出し側が渡す。commit は呼び出し側。V3 Binding PUT と Web 会話作成の唯一の書込口。V3.5: address が既存 session id と byte 一致なら新 session を作らず再利用。membership 不一致・複数・他 open binding の占有は `CreateGateBindingError::Conflict` |
| `canonical_session_id` | physical `extgate-{binding_id}` があればそれ、無ければ address と id が一致する再利用 session。どちらも無ければ None |

## sessions（webgate read 投影）

| 関数 | 契約 |
|---|---|
| `open_web_physical_session` | 開いている web binding の physical ID。address または physical ID で解決。同一 session に 2 件なら失敗 |
| `open_web_binding` | 開いている web binding の binding_id / instance_id / address。address または physical ID。同一 session に 2 件なら失敗 |
| `effective_agent_ids` | `agent_sessions` を join した実効参加者。open web binding があれば physical の membership |
| `project_session_row` | logical ID 維持。alias があれば表示属性は alias。無ければ physical。会話状態は physical。membership から participant を埋める |
| `list_sessions_page` | physical 行を除き logical 1 件。alias の無い open web binding も含める。各行に `agent_ids`。`updated_at DESC`。`limit` と `before` |
| `list_sessions` | テスト専用。投影なし全件 |

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
