# opencrab-actions

エージェント実行の集約口。載せ替え工程 4-a で Discord ゲートから移した
**判断**はここに置く。SkillEngine / conversation の実装は触らない。

## セッション inbound（`session_inbound`）

ゲートは正規化受信（本文・送信者の生識別子・対象 `session_id`）の束を
[`accept_inbound`] へ 1 回投げる。誰か・権限・standing・権限デバウンス・
trust 分割は core が決める。返すのは配送 effect（ターンした件）。
Discord と web が同じ入口を使う（web 専用の別経路は無い）。

| 関数 | 決めること |
|---|---|
| `accept_inbound` | 唯一の入り口。caller・DM/whitelist・#698 許可・standing・権限デバウンス・Q13 分割。`on_admitted` / `on_run` を呼ぶ |
| `delivery_effect` | ターン結果 → 本文 / NO_REPLY / Empty / Failed |
| `prepare_session_inbound` | `ensure_session` + inbound 記録（ロック前・#284）。Discord / `TranscriptSource` |
| `prepare_session_inbound_write` | 同じ順序（ensure → record）。web は行の形が現行 `session_logs`（`TranscriptSource` は使わない） |
| `start_session_turn` | 受信フック + 会話構築 + run |
| `run_session_turn` | resume / 継続（フックなし） |

`session_id` の書式はゲート規約のまま。表は動かさない。

## セッション watch ポリシー（`session_watch_policy`）

Nostr の `session_watches` 付きセッションだけ。ゲートは形（即時転送 / 束ね）だけを見る。
誰か・即応・権限毎デバウンスは `accept_inbound` が決める。

| 関数 | 決めること |
|---|---|
| `parse_session_watch_policy` | `'{}'` は未設定。非空は 4 クラス必須 |
| `watch_author_standing` | owner / followee / other。co_agent は other（即応を拡張しない） |

`interval_secs` の既定は持たない。0 は拒否。

## Discord チャンネル設定（`channel_config`）

`apply_discord_channel_config` が `discord_channel_config` の DB 書き込み
（Q12）。省略時 patch（#421）は移設前と同一。

露出（`definitions`）は Discord ゲートに残す。`SystemGatewayActions` には出さない。

## tool_logs（`BridgedExecutor`）

ツール実行が返った直後に 1 実行 1 行を書く。ゲートは書かない。
`outcome` は `done` / `failed` / `refused`（現行経路で出る 3 態）。
返り値と `memory_sessions` の既存記録は変えない。

## 関連

- [opencrab-discord](../discord/README.md)
