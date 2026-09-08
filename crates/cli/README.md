# cli

| バイナリ | 役割 |
|---|---|
| `opencrab-cli` | 対話 REPL。`sessions list` は `list_sessions_page`（physical `extgate-*` を隠す） |
| `opencrab-vacuum` | 停止中の手動 `VACUUM` |
| `opencrab-import-claude-code` | Claude Code 履歴の取り込み |
| `webgate-transplant` | 既存 web セッションを `extgate-{binding_id}` へ 14 store 一回移送。件数/digest 不一致で中止 |
