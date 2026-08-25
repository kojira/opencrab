# opencrab-nostr

Nostr ゲート。配送（購読・送信・passthrough）はここ。誰か・即応は core
（`opencrab-actions` の `session_inbound` / `session_watch_policy`）。

## 現行 `nostr-{agent}`

`session_watches` が 0 行なら従来どおり。`handle_event` は即時。ラベルは
`inbound_kind_label`（リポスト種別は足さない）。

## 新機構（`session_watches`）

行があるセッションだけ。ゲートは形だけを見る。

| 形 | 転送 |
|---|---|
| DM kind 4/1059 | 破棄（#514） |
| リプライ / メンション（e/p が当人）・リポスト 6/16・リアクション 7・長文 kind 30023（e/p が当人） | 即時 |
| タイムライン（自分宛でない kind 1 / 長文） | `interval_secs` で束ねて inbound 1 口 |
| 対話系を core が権限デバウンスした件 | core の `PrivilegeFire`（バッファと時限）。即時なら handle 時のみ prepare、時限発火時のみ prepare |

`interval_secs` は必須。既定値は無い。watch を置けるのは `nostr-` 系のみ（Q-B）。
設定口は `GET/POST /api/agents/{id}/nostr/watches`。
