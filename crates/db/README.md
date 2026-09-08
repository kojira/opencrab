# db

SQLite スキーマとクエリ。ゲートウェイはここを直接書かない。

## webgate_transplant

既存 `web-{agent_id}-*` セッションを V3 physical `extgate-{binding_id}` へ 14 store 一回移送する。

| 関数 | 契約 |
|---|---|
| `list_web_mappings` | 開いている `kind_id=web` binding から logical / physical / agent を列挙 |
| `validate_legacy_session` | prefix 一意・sole participant 一致。0/複数/不一致は失敗 |
| `snapshot_session` | 14 store の件数と、PK 順・非 session 列連結 SHA-256 |
| `transplant_mapping` | 1 TX。14 store の件数/digest 不一致は中止。再実行は marker + legacy 参照 0 + 保存済み physical inventory 一致のときだけ write-zero |
| `transplant_all` | 全 mapping を順に移送 |

CLI: `webgate-transplant <db-path>`（`crates/cli`）。operator 敷設は `scripts/webgate-provision`。
