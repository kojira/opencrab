use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Agent MCP Config（per-agent の MCP サーバ設定。1 エージェント × 複数サーバ）
// ============================================
//
// args / env は JSON TEXT で保持し、server 層で McpServerConfig にパースする
// （db クレートは opencrab-mcp に依存しない）。主キーは (agent_id, name)。

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMcpServerRow {
    pub agent_id: String,
    /// 論理名（ツール名プレフィックスに使う）。
    pub name: String,
    pub command: String,
    /// 引数の JSON 配列（例 `["-y","@scope/server"]`）。
    pub args_json: String,
    /// 追加環境変数の JSON オブジェクト。
    pub env_json: String,
    /// true なら owner/trusted のターンでのみ使える。
    pub trusted_only: bool,
    pub enabled: bool,
}

pub fn upsert_agent_mcp_server(conn: &Connection, row: &AgentMcpServerRow) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_mcp_config (agent_id, name, command, args_json, env_json, trusted_only, enabled, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(agent_id, name) DO UPDATE SET
            command = excluded.command,
            args_json = excluded.args_json,
            env_json = excluded.env_json,
            trusted_only = excluded.trusted_only,
            enabled = excluded.enabled,
            updated_at = excluded.updated_at",
        params![
            row.agent_id,
            row.name,
            row.command,
            row.args_json,
            row.env_json,
            row.trusted_only,
            row.enabled,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn row_from(r: &rusqlite::Row) -> rusqlite::Result<AgentMcpServerRow> {
    Ok(AgentMcpServerRow {
        agent_id: r.get(0)?,
        name: r.get(1)?,
        command: r.get(2)?,
        args_json: r.get(3)?,
        env_json: r.get(4)?,
        trusted_only: r.get(5)?,
        enabled: r.get(6)?,
    })
}

const COLS: &str = "agent_id, name, command, args_json, env_json, trusted_only, enabled";

pub fn list_agent_mcp_servers(conn: &Connection, agent_id: &str) -> Result<Vec<AgentMcpServerRow>> {
    let sql = format!("SELECT {COLS} FROM agent_mcp_config WHERE agent_id = ?1 ORDER BY name");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![agent_id], row_from)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn get_agent_mcp_server(
    conn: &Connection,
    agent_id: &str,
    name: &str,
) -> Result<Option<AgentMcpServerRow>> {
    let sql = format!("SELECT {COLS} FROM agent_mcp_config WHERE agent_id = ?1 AND name = ?2");
    let result = conn.query_row(&sql, params![agent_id, name], row_from);
    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete_agent_mcp_server(conn: &Connection, agent_id: &str, name: &str) -> Result<bool> {
    let deleted = conn.execute(
        "DELETE FROM agent_mcp_config WHERE agent_id = ?1 AND name = ?2",
        params![agent_id, name],
    )?;
    Ok(deleted > 0)
}

pub fn set_agent_mcp_server_enabled(
    conn: &Connection,
    agent_id: &str,
    name: &str,
    enabled: bool,
) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE agent_mcp_config SET enabled = ?1, updated_at = ?2 WHERE agent_id = ?3 AND name = ?4",
        params![enabled, Utc::now().to_rfc3339(), agent_id, name],
    )?;
    Ok(updated > 0)
}

/// 全エージェント分の enabled なサーバを列挙する（起動時 restore 用）。
pub fn list_all_enabled_agent_mcp_servers(conn: &Connection) -> Result<Vec<AgentMcpServerRow>> {
    let sql =
        format!("SELECT {COLS} FROM agent_mcp_config WHERE enabled = 1 ORDER BY agent_id, name");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_from)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        crate::init_memory().unwrap()
    }

    fn row(agent: &str, name: &str, enabled: bool) -> AgentMcpServerRow {
        AgentMcpServerRow {
            agent_id: agent.to_string(),
            name: name.to_string(),
            command: "npx".to_string(),
            args_json: r#"["-y","@scope/server"]"#.to_string(),
            env_json: r#"{"TOKEN":"x"}"#.to_string(),
            trusted_only: true,
            enabled,
        }
    }

    #[test]
    fn test_multi_server_crud_and_enabled_filter() {
        let conn = mem();
        upsert_agent_mcp_server(&conn, &row("a1", "fs", true)).unwrap();
        upsert_agent_mcp_server(&conn, &row("a1", "gh", false)).unwrap();
        upsert_agent_mcp_server(&conn, &row("a2", "fs", true)).unwrap();

        // agent 単位の列挙。
        let a1 = list_agent_mcp_servers(&conn, "a1").unwrap();
        assert_eq!(a1.len(), 2);
        assert_eq!(a1[0].name, "fs"); // name 順
        assert_eq!(a1[1].name, "gh");

        // get.
        let got = get_agent_mcp_server(&conn, "a1", "fs").unwrap().unwrap();
        assert_eq!(got.command, "npx");
        assert!(got.trusted_only);
        assert!(get_agent_mcp_server(&conn, "a1", "missing")
            .unwrap()
            .is_none());

        // enabled 列挙（全エージェント）は enabled=1 の2件（a1/fs, a2/fs）。
        let en = list_all_enabled_agent_mcp_servers(&conn).unwrap();
        assert_eq!(en.len(), 2);

        // enable 切替。
        assert!(set_agent_mcp_server_enabled(&conn, "a1", "gh", true).unwrap());
        assert_eq!(list_all_enabled_agent_mcp_servers(&conn).unwrap().len(), 3);

        // upsert で同じ (agent,name) は更新（重複行を作らない）。
        let mut r = row("a1", "fs", true);
        r.command = "node".to_string();
        upsert_agent_mcp_server(&conn, &r).unwrap();
        assert_eq!(list_agent_mcp_servers(&conn, "a1").unwrap().len(), 2);
        assert_eq!(
            get_agent_mcp_server(&conn, "a1", "fs")
                .unwrap()
                .unwrap()
                .command,
            "node"
        );

        // delete.
        assert!(delete_agent_mcp_server(&conn, "a1", "fs").unwrap());
        assert!(get_agent_mcp_server(&conn, "a1", "fs").unwrap().is_none());
        assert_eq!(list_agent_mcp_servers(&conn, "a1").unwrap().len(), 1);
    }
}
