//! `queries` の回帰テスト。検証対象の queries 子モジュール（ドメイン）ごとに分けてある。
//!
//! - 各ファイルは同名の queries モジュール（`agents` / `sessions` / `skills` …）を検証する。
//! - `heartbeat` は heartbeat log・指示文・agent/channel の間隔解決をまとめて持つ。
//! - `memory_index/` は memory_index 配下（short_id・FTS・カテゴリ層・整理ラン・宣言ユニット）。
//!
//! ここで共有するのは in-memory DB を開く `setup()` だけ。モジュール専用のヘルパは各自が持つ。

use rusqlite::{params, Connection};

#[allow(unused_imports)]
use super::*;

mod agent_discord_config;
mod agent_inbox;
mod agents;
mod channel_config;
mod curated_memory;
mod heartbeat;
mod impressions;
mod llm_metrics;
mod memory_index;
mod model_pricing;
mod session_logs;
mod sessions;
mod skills;
mod task_ledger;
mod trusted_users;
mod webhook_config;

fn setup() -> Connection {
    crate::init_memory().expect("failed to init in-memory DB")
}
