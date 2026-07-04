use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Heartbeat Instructions（ハートビート指示）
// ============================================

/// ハートビート指示が未設定のときに使う既定文言（後方互換）。
/// 出力形式の規約（SPEAK/LEARN/IDLE）はランタイム側で固定するため、ここには含めない。
pub const DEFAULT_HEARTBEAT_INSTRUCTIONS: &str =
    "今この瞬間、自律的に何をするか判断してください。発言は30分に1回以下が望ましい。";

/// ハートビート指示の最大文字数。
pub const MAX_HEARTBEAT_INSTRUCTIONS_LEN: usize = 4000;

/// 指示本文をサニタイズする。制御文字（改行・タブを除く）を除去し、最大長でクランプする。
pub fn sanitize_heartbeat_instructions(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect();
    cleaned
        .chars()
        .take(MAX_HEARTBEAT_INSTRUCTIONS_LEN)
        .collect()
}

/// ハートビート指示の解決結果。`source` はどの設定が使われたかを示す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHeartbeatInstructions {
    pub text: String,
    /// "default" | "agent" | "channel" | "agent+channel"
    pub source: &'static str,
}

/// ハートビートtickのプロンプト本文を解決する。
///
/// 優先順位（§4.2）:
///  1. `(channel_id, agent_id)` のチャンネル上書き
///  2. `(channel_id, "")` のグローバルチャンネル上書き
///  3. `agents.heartbeat_instructions`（エージェント全体）
///  4. 既定文言
///
/// 合成方針: エージェント指示とチャンネル上書きが両方あれば連結する（チャンネル側が後）。
pub fn resolve_heartbeat_instructions(
    conn: &Connection,
    agent_id: &str,
    channel_id: &str,
) -> ResolvedHeartbeatInstructions {
    let agent_global = get_agent(conn, agent_id)
        .ok()
        .flatten()
        .map(|a| sanitize_heartbeat_instructions(&a.heartbeat_instructions))
        .unwrap_or_default();

    // channel(agent) override → channel(global) override
    let channel_override = {
        let per_agent = get_channel_config_for_agent(conn, channel_id, agent_id)
            .ok()
            .flatten()
            .map(|c| sanitize_heartbeat_instructions(&c.heartbeat_instructions))
            .filter(|s| !s.is_empty());
        match per_agent {
            Some(s) => s,
            None => get_channel_config_for_agent(conn, channel_id, "")
                .ok()
                .flatten()
                .map(|c| sanitize_heartbeat_instructions(&c.heartbeat_instructions))
                .filter(|s| !s.is_empty())
                .unwrap_or_default(),
        }
    };

    let has_agent = !agent_global.is_empty();
    let has_channel = !channel_override.is_empty();

    match (has_agent, has_channel) {
        (false, false) => ResolvedHeartbeatInstructions {
            text: DEFAULT_HEARTBEAT_INSTRUCTIONS.to_string(),
            source: "default",
        },
        (true, false) => ResolvedHeartbeatInstructions {
            text: agent_global,
            source: "agent",
        },
        (false, true) => ResolvedHeartbeatInstructions {
            text: channel_override,
            source: "channel",
        },
        (true, true) => ResolvedHeartbeatInstructions {
            text: format!("{agent_global}\n\n{channel_override}"),
            source: "agent+channel",
        },
    }
}

/// ハートビート指示の監査ログ1件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatInstructionsAuditRow {
    pub agent_id: String,
    pub scope: String,
    pub channel_id: Option<String>,
    pub caller_identity: String,
    pub caller_discord_id: Option<String>,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub reason: Option<String>,
}

/// 監査ログを記録する。
pub fn insert_heartbeat_instructions_audit(
    conn: &Connection,
    audit: &HeartbeatInstructionsAuditRow,
) -> Result<()> {
    conn.execute(
        "INSERT INTO heartbeat_instructions_audit
            (agent_id, scope, channel_id, caller_identity, caller_discord_id, old_value, new_value, reason, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            audit.agent_id,
            audit.scope,
            audit.channel_id,
            audit.caller_identity,
            audit.caller_discord_id,
            audit.old_value,
            audit.new_value,
            audit.reason,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// エージェントの監査ログを新しい順に取得する（テスト・確認用）。
pub fn list_heartbeat_instructions_audit(
    conn: &Connection,
    agent_id: &str,
    limit: i64,
) -> Result<Vec<HeartbeatInstructionsAuditRow>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, scope, channel_id, caller_identity, caller_discord_id, old_value, new_value, reason
         FROM heartbeat_instructions_audit WHERE agent_id = ?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![agent_id, limit], |row| {
        Ok(HeartbeatInstructionsAuditRow {
            agent_id: row.get(0)?,
            scope: row.get(1)?,
            channel_id: row.get(2)?,
            caller_identity: row.get(3)?,
            caller_discord_id: row.get(4)?,
            old_value: row.get(5)?,
            new_value: row.get(6)?,
            reason: row.get(7)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}
