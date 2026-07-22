//! OpenCrab の MCP（Model Context Protocol）クライアント。
//!
//! エージェントが外部 MCP サーバ（stdio subprocess）のツールを使えるようにする。
//! Nostr/Discord ゲートウェイと同じく per-agent 設定（DB）で、どのサーバを有効化するかを
//! 指定する。プロトコルは薄い自前実装（[`client::McpConnection`]）で、`initialize` /
//! `tools/list` / `tools/call` を話す。
//!
//! - [`config::McpServerConfig`] — サーバ1つの起動設定（command/args/env、trusted_only）。
//! - [`client::McpClient`] — 起動済みサーバへの接続（tools キャッシュ + `tools/call`）。
//!
//! ツール名は `mcp__<server>__<tool>` で名前空間化して組み込みツールと衝突させない
//! （[`config::qualified_tool_name`] / [`config::split_tool_name`]）。

pub mod client;
pub mod config;

pub use client::{McpClient, McpConnection, McpTool, McpToolResult};
pub use config::{
    config_from_row, is_valid_server_name, qualified_tool_name, split_tool_name, McpServerConfig,
    MCP_TOOL_PREFIX,
};
