# opencrab-nostr

Nostr ゲート。配送（購読・送信・passthrough）はここ。誰か・即応は core
（`opencrab-actions` の `session_inbound` / `session_watch_policy`）。

## core adapter（`adapter.rs`）

元栓（allow-set・DM・自己投稿）を `accept_inbound` の記録 callback 前に評価する。
default / watch Immediate とも同一口で `accept_inbound` exact 1 回。`route` は輸送の正
（再分類しない）。V3 said は `admit_nostr_said`（不正アンカーは拒否・記録 0）。
Bundle は V1 の次行 `[NOSTRBUNDLE/V1 [origin…]]` を検証し、core `nostr_bundle_state`
（v45）が member を束ねる。PrivilegeFire は Immediate だけ。

## 段階移行（`ingress.rs`）

`gate.nostr_ingress` = `legacy`（既定）| `v3_shadow` | `v3`。未知値は起動失敗。
旧 in-process ループは削除しない。

- `v3`: instance/binding を敷設し、旧ループは止めて TimedFire と allow-set 300 秒更新だけ残す。
  identity 切替は停止→revision +1→再起動（hot swap しない）。
- `v3_shadow`: 旧ループを回す。instance 行だけ敷く。本番 UDS への接続・hello・bind ack・
  live 占有はしない。Binding PUT / said / say はしない。同一 JSONL を legacy / gateway の
  両 parser と分類でメモリ内照合する（default lane は gateway Immediate。DM は
  legacy Discard / gateway Immediate を一致扱い）。

## binding（`binding.rs`）

address = 既存 session_id。`nostr-{agent}` に watch があれば default lane なし。
1 session N watch は 1 binding + N lane（id ASC）。

## 現行 `nostr-{agent}`

`session_watches` が 0 行なら従来どおり。`handle_event` は adapter 経由の即時。ラベルは
`inbound_kind_label`（リポスト種別は足さない）。

## 新機構（`session_watches`）

行があるセッションだけ。ゲートは形だけを見る。

| 形 | 転送 |
|---|---|
| DM kind 4/1059 | 破棄（#514） |
| リプライ / メンション（e/p が当人）・リポスト 6/16・リアクション 7・長文 kind 30023（e/p が当人） | 即時 |
| タイムライン（自分宛でない kind 1 / 長文） | V3: gateway が `interval_secs` で flush し、core `NostrBundleCoordinator` が全 member 記録 + turn 0/1。legacy: 同 interval で inbound 1 口 |
| 対話系を core が権限デバウンスした件 | core の `PrivilegeFire`（バッファと時限）。即時なら handle 時のみ prepare、時限発火時のみ prepare |

`interval_secs` は必須。既定値は無い。watch を置けるのは `nostr-` 系のみ（Q-B）。
設定口は `GET/POST /api/agents/{id}/nostr/watches`。
