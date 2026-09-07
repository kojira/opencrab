use std::path::PathBuf;
pub(crate) use std::sync::{Arc, Mutex, Once, OnceLock};
pub(crate) use std::time::Duration;

pub(crate) use axum::body::Body;
pub(crate) use axum::http::{header, Request, StatusCode};
pub(crate) use http_body_util::BodyExt;
pub(crate) use tower::ServiceExt;

pub(crate) use opencrab_llm::message::*;
pub(crate) use opencrab_llm::router::LlmRouter;
pub(crate) use opencrab_llm::traits::LlmProvider;
pub(crate) use opencrab_server::AppState;

pub(crate) use opencrab_discord_gateway::config::InstancePlacement;
pub(crate) use opencrab_discord_gateway::harness::HarnessOverrides;
pub(crate) use opencrab_discord_gateway::run::spawn_instance;
pub(crate) use opencrab_extgate::{
    admin_router, resolve_caller_identity_with_owner, serve_uds, ExtgateState, OperatorToken,
};
pub(crate) use opencrab_gate_client::client::InstanceClient;

pub(crate) use tracing_subscriber::layer::{Context, SubscriberExt};
pub(crate) use tracing_subscriber::Layer;

pub(crate) const TOKEN: &str = "operator-token-discord-qc";
pub(crate) const AGENT_ID: &str = "agent-discord-qc";
pub(crate) const GUILD: &str = "500";
pub(crate) const CHANNEL: &str = "600";
/// #915: typing 隔離テスト専用チャンネル（他テストは 600 のみ使う）。並列 CI でも typing を分離。
pub(crate) const CHANNEL_TY: &str = "601";
pub(crate) const SELF_BOT: &str = "111"; // bot 自身の user id（自分の投稿除外）。
pub(crate) const AUTHOR: &str = "222"; // owner の Discord user id（generic admission で caller=Owner）。
/// dry-run を拾う tracing target（= `opencrab_discord_gateway::transport::DRY_RUN_LOG_TARGET`）。
pub(crate) const DRY_RUN_TARGET: &str = "opencrab_discordgate::dry_run";
pub(crate) const SYS_ACCEPTED: &str = "👀";
pub(crate) const SYS_COMPLETED: &str = "🏁";

pub(crate) fn address() -> String {
    format!("discord-{AGENT_ID}-{GUILD}-{CHANNEL}")
}

mod capture;
mod fixture;
mod folding_mock;
mod harness;
mod responses;
mod routed_mock;

pub(crate) use capture::*;
pub(crate) use fixture::*;
pub(crate) use folding_mock::*;
pub(crate) use harness::*;
pub(crate) use responses::*;
pub(crate) use routed_mock::*;
