//! Discord gateway フェーズ1 のオフライン E2E（DESIGN-DISCORD-GATE v17）。
//!
//! トークン・ネットワーク・serenity 接続なしで **実配線** を通す決定的 E2E:
//!   実 `serve_uds`（extgate core）＋ 実 `AppState`（mock LLM）
//!     ⇕ 実 UDS ⇕
//!   実 `discord-gateway::spawn_instance`（fake_events 注入 ＋ dry-run 送信の両有効）
//!
//! 観測channel = dry-run の tracing ログ（target = `opencrab_discordgate::dry_run`）。kind で say /
//! reply / reaction を区別する。単一スレッド（`--test-threads=1`）前提。
//!
//! 検証:
//! - (a) 受信 Discord message → said → turn → say（通常投稿）が dry-run に出る。会話に **e1**（§9A
//!   e番号・core 汎化が discord kind へも採番）が現れる。
//! - (b) reply(e1, 本文) の実 DI 経路: LLM tool_call → core が e1→origin 解決 → invoke → gateway が
//!   REST（dry-run）→ 決着。dry-run に kind="reply" が出る。
//! - (c) reaction(e1, emoji) の実 DI 経路。dry-run に kind="reaction"・emoji が出る。
//!
//! 非 nostr kind の said admission は generic 経路（whitelist + owner/dm）を通る（nostr hooks は張らない）。

mod discord_qc_harness;
