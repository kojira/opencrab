# schema（`PRAGMA user_version` + 番号付き MIGRATIONS）

新規 DB は `sql.rs` の `SCHEMA_SQL`。既存 DB は `mod.rs` の `MIGRATIONS`。
両方に同じ差分を書く（新規 DB は最新番号を通らず SCHEMA だけ、が本体の罠）。

## v43（載せ替え工程 3）

場＝セッション（同一概念）。表は `sessions` / `agent_sessions` のまま。データ移動ゼロ。

| 変更 | 内容 |
|---|---|
| `sessions.policy_json` | `TEXT NOT NULL DEFAULT '{}'`。未設定＝現行挙動維持。列名は session のまま（値は場 ID） |
| `agent_sessions` | 列を足さない |
| `session_watches` | 場に紐づく Nostr 購読（1 場 N 行）。`interval_secs` 必須・`CHECK (> 0)` |
| `tool_logs` | ツール 1 実行 1 行。`outcome CHECK (done\|failed\|refused\|deadline\|stopped)` |

やってはいけないこと: 既存表への INSERT、既存行の UPDATE、DROP、列改名、VIEW、`places` / `memberships`。

本番コピー検証: `scripts/verify-v43-transplant-copy.sh`（`OPENCRAB_REHEARSAL_DB`、sqlite3 `.backup` のみでソースを読む）。
