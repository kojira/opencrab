# opencrab-discord

Discord ゲート。載せ替え工程 4-a 以降、**配送専用**。

会話の単位はセッションのまま（`sessions` / `agent_sessions`）。`session_id` は
`discord-{agent_id}-{guild_id}-{channel_id}`（DM は guild が空で
`discord-{agent_id}--{channel_id}`）。書式も表も変えない。

## 残すもの（配送）

- 受信（serenity）→ core へ正規化イベントを渡す
- core の決定（本文 / NO_REPLY / A2UI 構造）の送信
- 2 秒デバウンス（形＋時刻。権限分割は core）
- Discord API ツール（list_guilds/channels、reaction、webhook/channel 作成、send_file、voice join/leave）
- スノーフレーク ID の数値→文字列正規化（`normalize_id_args`）
- A2UI 描画（`renderer` / `a2ui_surface`）、`form_modal`、typing、webhook
- 送信前の `is_channel_writable`（送信可否。判断ではない）

## 移した判断（core = `opencrab-actions`）

| 判断 | 入口 |
|---|---|
| trust_level による run 分割 | `plan_record_only_flags` / `consecutive_trust_groups` |
| DM / whitelist の落とす・通す | `admit_inbound_message` / `admit_inbound_agent` |
| セッション確保 + inbound 記録 | `prepare_session_inbound` |
| ターン起動 | `start_session_turn` / `run_session_turn` |
| `discord_channel_config` の DB 書き | `apply_discord_channel_config`（definitions と execute 受け口はここ） |

信頼の**計算**（`resolve_caller` / `dm_allowed*`）は `AgentRunner`（server）のまま。
core は計算結果を受けて決定する。`SystemGatewayActions` に `discord_channel_config`
は出さない（web / Nostr へ波及させない）。

## 関連

- [docs/discord.md](../../docs/discord.md)
- [opencrab-actions](../actions/README.md)
