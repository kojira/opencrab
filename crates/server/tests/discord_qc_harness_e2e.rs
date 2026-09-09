//! Discord gateway フェーズ1 のオフライン E2E（DESIGN-DISCORD-GATE v17）。
//!
//! トークン・ネットワーク・serenity 接続なしで実配線を通す決定的 E2E。

#[path = "discord_qc_harness/support/mod.rs"]
mod support;
use support::*;

include!("discord_qc_harness/base_delivery.rs");
include!("discord_qc_harness/chunking.rs");
include!("discord_qc_harness/completion_continue.rs");
include!("discord_qc_harness/completion_edges.rs");
include!("discord_qc_harness/completion_lifecycle.rs");
include!("discord_qc_harness/folding.rs");
include!("discord_qc_harness/heartbeat.rs");
include!("discord_qc_harness/holding_resume.rs");
include!("discord_qc_harness/no_reply.rs");
include!("discord_qc_harness/read_reactions.rs");
include!("discord_qc_harness/tool_completion_turn.rs");
