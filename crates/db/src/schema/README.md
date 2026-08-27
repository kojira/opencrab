# schema（`PRAGMA user_version` + 番号付き MIGRATIONS）

新規 DB は `sql.rs` の `SCHEMA_SQL`。既存 DB は `mod.rs` の `MIGRATIONS`。
両方に同じ差分を書く（新規 DB は最新番号を通らず SCHEMA だけ、が本体の罠）。

## v43（載せ替え工程 3）

会話の単位はセッション。表は `sessions` / `agent_sessions` のまま。データ移動ゼロ。

| 変更 | 内容 |
|---|---|
| `sessions.policy_json` | `TEXT NOT NULL DEFAULT '{}'`。未設定＝現行挙動維持。 |
| `agent_sessions` | 列を足さない |
| `session_watches` | セッションに紐づく Nostr 購読（1 セッション N 行）。`interval_secs` 必須・`CHECK (> 0)` |
| `tool_logs` | ツール 1 実行 1 行。`outcome CHECK (done\|failed\|refused\|deadline\|stopped)` |

やってはいけないこと: 既存表への INSERT、既存行の UPDATE、DROP、列改名、VIEW。
表集合は期待一覧（適用前 ∪ `session_watches` / `tool_logs`）と一致させる。

本番コピー検証: `scripts/verify-v43-transplant-copy.sh`（`OPENCRAB_REHEARSAL_DB`、sqlite3 `.backup` のみでソースを読む）。

## v45（Nostr Bundle coordinator）

V3 の 4 表・wire/admin 契約には足さない。Nostr 固有表 `nostr_bundle_state` だけを所有する。

| 列 | 内容 |
|---|---|
| `binding_id`, `bundle_id` | PRIMARY KEY |
| `manifest_json` | index 順の member origin |
| `received_bits` / `new_admitted_bits` | 長さ = count の `0`/`1` 列 |
| `completed` | `0`/`1` |

既存 DB は番号付き migration、新規 DB は `SCHEMA_SQL`。両方に同じ `CREATE TABLE IF NOT EXISTS`。
