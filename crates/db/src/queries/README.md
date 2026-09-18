# queries

ドメイン別の行型と SQL。`mod.rs` から再輸出する。

## nostr instance 敷設

Nostr の instance/binding 書き込みは `crates/server/src/nostr_provision.rs`。
`create_gate_binding_in_tx` を使い、address=既存 session_id で V3.5 reuse する。
session 不在・membership 不一致は fail-loud。

## gate_binding

| 関数 | 契約 |
|---|---|
| `create_gate_binding_in_tx` | physical session・sole membership・open bindingを1 TXで作る汎用部品。themeは呼び出し側が渡し、commitも呼び出し側が行う。addressが既存session IDとbyte一致なら再利用する。membership不一致・複数・他open bindingの占有は`CreateGateBindingError::Conflict` |
| `canonical_session_id` | physical `extgate-{binding_id}` があればそれ、無ければ address と id が一致する再利用 session。どちらも無ければ None |
| `lookup_canonical_gate_binding` | canonical session ID から open・未削除の generic binding を exact 解決。0件=`NotFound`、複数=`Ambiguous`。address 候補は v51 の address-first partial index を使う |

## sessions

| 関数 | 契約 |
|---|---|
| `effective_agent_ids` | opaque session IDをそのまま使い、`agent_sessions`をjoinした実効参加者を返す |
| `list_sessions_page` | 全sessionを`updated_at DESC, id DESC`で返す。`limit`とopaqueな`before`を受ける |
| `list_sessions` | テスト専用の全件一覧 |

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
