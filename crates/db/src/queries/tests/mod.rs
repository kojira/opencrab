//! `queries` の回帰テスト。分割ファイルはこの `tests` モジュールへ直接 include し、
//! 分割前の fully-qualified test path を維持する。

use rusqlite::{params, Connection};

#[allow(unused_imports)]
use super::*;

include!("memory_index/fixtures.rs");
include!("sessions.rs");
include!("agents.rs");
include!("agent_discord_config.rs");
include!("agent_inbox.rs");
include!("channel_config.rs");
include!("curated_memory.rs");
include!("heartbeat.rs");
include!("impressions.rs");
include!("llm_metrics.rs");
include!("memory_index/category.rs");
include!("memory_index/declared_unit.rs");
include!("memory_index/nodes_fts.rs");
include!("memory_index/organize.rs");
include!("memory_index/short_id.rs");
include!("model_pricing.rs");
include!("session_logs.rs");
include!("skills.rs");
include!("task_ledger.rs");
include!("trusted_users.rs");
include!("webhook_config.rs");

fn setup() -> Connection {
    crate::init_memory().expect("failed to init in-memory DB")
}
