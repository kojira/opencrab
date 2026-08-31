//! Discord gateway V3 profile（DESIGN-DISCORD-GATE.md v17 フェーズ1）。
//!
//! 1 bot credential = 1 V3 instance = 1 gateway process = exact 1 agent（設計 §0）。
//! gate wire（hello/bind/said/say/invoke）は [`opencrab_gate_client`] を Nostr gateway と同一に
//! 経由する。core への Discord 固有追加はゼロで、能力は hello の `GatewayOperationDeclaration`
//! として宣言する（reply/reaction/resolve）。会話の e/u/c 採番は core conversation.rs の汎用機構を
//! 再利用し、gateway は said に生 origin/author を載せるだけ（core に Discord 語彙を足さない）。
//!
//! 秘密（bot token）は gateway process env のみ（[`secret`]）。QC は fixture 注入 + dry-run の
//! オフライン E2E（[`harness`]）で実配線を回す（Nostr の偽watch 相当）。

pub mod config;
pub mod harness;
pub mod map;
pub mod ops;
pub mod post;
pub mod receive;
pub mod run;
pub mod secret;
pub mod transport;
