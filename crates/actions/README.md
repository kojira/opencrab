# opencrab-actions

エージェント実行の集約口。載せ替え工程 4-a で Discord ゲートから移した
**判断**はここに置く。SkillEngine / conversation の実装は触らない。

## セッション inbound（`session_inbound`）

ゲートは正規化受信（本文・送信者の生識別子・対象 `session_id`）だけを渡す。

| 関数 | 決めること |
|---|---|
| `admit_inbound_message` | DM 事前ゲート（落とす/通す） |
| `admit_inbound_agent` | 個別 DM / チャンネル whitelist |
| `consecutive_trust_groups` / `plan_record_only_flags` | trust_level 分割。本数と caller は現行同一（Q13） |
| `prepare_session_inbound` | `ensure_session` + inbound 記録（ロック前・#284） |
| `start_session_turn` | 受信フック + 会話構築 + run |
| `run_session_turn` | resume / 継続（フックなし） |

`session_id` の書式はゲート規約のまま。表は動かさない。

## Discord チャンネル設定（`channel_config`）

`apply_discord_channel_config` が `discord_channel_config` の DB 書き込み
（Q12）。省略時 patch（#421）は移設前と同一。

露出（`definitions`）は Discord ゲートに残す。`SystemGatewayActions` には出さない。

## 関連

- [opencrab-discord](../discord/README.md)
