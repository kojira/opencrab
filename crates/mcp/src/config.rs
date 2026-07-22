//! MCP サーバ設定（per-agent）。DB 行は JSON TEXT で持ち、ここで型へパースする
//! （db クレートは opencrab-mcp に依存しない）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// MCP ツール名のプレフィックス。組み込みツールと衝突させないため
/// `mcp__<server>__<tool>` で名前空間を切る。
pub const MCP_TOOL_PREFIX: &str = "mcp__";
pub const MCP_TOOL_SEP: &str = "__";

/// エージェント1体が使う MCP サーバ1つの設定（stdio subprocess）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// 論理名（ツール名プレフィックスに使う）。英数字・`_`・`-` のみ。
    pub name: String,
    /// 起動コマンド（例 `npx`）。
    pub command: String,
    /// 引数（例 `["-y", "@modelcontextprotocol/server-filesystem", "/path"]`）。
    #[serde(default)]
    pub args: Vec<String>,
    /// 追加環境変数。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// true（既定）なら owner/trusted 起点のターンでのみ使える（外部ユーザー起点の
    /// Agent ターンには出さない＝プロンプトインジェクションで外部システムを叩かせない）。
    #[serde(default = "default_true")]
    pub trusted_only: bool,
    /// 有効か。
    #[serde(default)]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// サーバ名が安全か（英数字・`_`・`-`、1〜64 文字）。ツール名生成に使うため制限する。
pub fn is_valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `mcp__<server>__<tool>` を組み立てる。
pub fn qualified_tool_name(server: &str, tool: &str) -> String {
    format!("{MCP_TOOL_PREFIX}{server}{MCP_TOOL_SEP}{tool}")
}

/// `mcp__<server>__<tool>` を (server, tool) に分解する。MCP ツールでなければ None。
/// tool 側に `__` が含まれても、最初の区切りで server を切り出し残りを tool とする。
pub fn split_tool_name(qualified: &str) -> Option<(&str, &str)> {
    let rest = qualified.strip_prefix(MCP_TOOL_PREFIX)?;
    let idx = rest.find(MCP_TOOL_SEP)?;
    let server = &rest[..idx];
    let tool = &rest[idx + MCP_TOOL_SEP.len()..];
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

/// DB 行（args_json / env_json）を [`McpServerConfig`] へパースする。壊れた JSON は
/// 空にフォールバックする。
pub fn config_from_row(row: &opencrab_db::queries::AgentMcpServerRow) -> McpServerConfig {
    let args: Vec<String> = serde_json::from_str(&row.args_json).unwrap_or_default();
    let env: BTreeMap<String, String> = serde_json::from_str(&row.env_json).unwrap_or_default();
    McpServerConfig {
        name: row.name.clone(),
        command: row.command.clone(),
        args,
        env,
        trusted_only: row.trusted_only,
        enabled: row.enabled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_server_name() {
        assert!(is_valid_server_name("filesystem"));
        assert!(is_valid_server_name("gh-mcp_1"));
        assert!(!is_valid_server_name(""));
        assert!(!is_valid_server_name("a/b"));
        assert!(!is_valid_server_name("a b"));
        assert!(!is_valid_server_name("a.b"));
        assert!(!is_valid_server_name(&"x".repeat(65)));
    }

    #[test]
    fn test_qualify_and_split() {
        let q = qualified_tool_name("fs", "read_file");
        assert_eq!(q, "mcp__fs__read_file");
        assert_eq!(split_tool_name(&q), Some(("fs", "read_file")));
        // tool 側に __ があっても server は最初の区切りで確定。
        assert_eq!(
            split_tool_name("mcp__fs__read__file"),
            Some(("fs", "read__file"))
        );
        // MCP ツールでないものは None。
        assert_eq!(split_tool_name("read_file"), None);
        assert_eq!(split_tool_name("mcp__only"), None);
        assert_eq!(split_tool_name("mcp____tool"), None);
    }
}
