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
