//! nostaro（自作 Nostr CLI）を subprocess 制御するラッパー。
//!
//! codex/cursor プロバイダと同じ「別コマンドを spawn して制御」パターン。鍵の共有
//! 事故を防ぐため、エージェント毎に **一意な config パス**（`data/agents/{id}/nostr/
//! config.toml`）を `--config` で明示指定する（`resolve_agent_workspace` と同じ検証
//! 経路で組む）。リレー/フィルタは watch のフラグで渡し、nostaro の config 側 default
//! に依存しない（指定リレー以外に繋がせない）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use opencrab_core::secret_box;
use opencrab_core::workspace::resolve_agent_workspace;
use tokio::process::Command;
use tokio::sync::Semaphore;
use zeroize::Zeroizing;

use crate::config::NostrConfig;

include!("cli/keys_config.rs");
include!("cli/commands.rs");
include!("cli/security_parsing.rs");

#[cfg(test)]
mod tests {
    use super::*;

    include!("cli/tests/paths_keys_config.rs");
    include!("cli/tests/watch.rs");
    include!("cli/tests/passthrough_parsing.rs");
}
