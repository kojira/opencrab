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

// ============================================
// Agent Heartbeat Config（エージェント単位のハートビート設定 / #247）
// ============================================

/// エージェント単位のハートビート設定 1 行（`agent_heartbeat_config`）。
///
/// **チャンネル単位の設定（`discord_channel_config`）とは別物**。発火の判定を
/// どちらから引くかの切り替えは段階 3（別 issue）で、この版では「エージェントが
/// 自分の設定を持てる」ところまで。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHeartbeatConfigRow {
    pub agent_id: String,
    pub enabled: bool,
    /// 生値。`None` = 未設定（運用者の既定に従う）。
    ///
    /// **`u64` にしない。** 0 や負値のような壊れた値を `as u64` で巨大な正の数へ
    /// 化けさせず、そのまま `resolve_agent_heartbeat` の fail-closed 判定へ渡す。
    pub interval_secs: Option<i64>,
}

/// 設定行を取得する。行が無ければ `None`。
pub fn get_agent_heartbeat_config(
    conn: &Connection,
    agent_id: &str,
) -> Result<Option<AgentHeartbeatConfigRow>> {
    let result = conn.query_row(
        "SELECT agent_id, enabled, interval_secs FROM agent_heartbeat_config WHERE agent_id = ?1",
        params![agent_id],
        |row| {
            Ok(AgentHeartbeatConfigRow {
                agent_id: row.get(0)?,
                enabled: row.get(1)?,
                interval_secs: row.get(2)?,
            })
        },
    );
    match result {
        Ok(cfg) => Ok(Some(cfg)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 設定行を作成/更新する。
pub fn upsert_agent_heartbeat_config(
    conn: &Connection,
    cfg: &AgentHeartbeatConfigRow,
) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_heartbeat_config (agent_id, enabled, interval_secs, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(agent_id) DO UPDATE SET
            enabled = excluded.enabled,
            interval_secs = excluded.interval_secs,
            updated_at = excluded.updated_at",
        params![
            cfg.agent_id,
            cfg.enabled,
            cfg.interval_secs,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// エージェント単位ハートビート設定の解決結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgentHeartbeat {
    pub enabled: bool,
    /// 実際に使う間隔（秒）。`enabled = false` のときも「有効にしたらこうなる」値を返す。
    pub interval_secs: u64,
    /// どこから来た値か。
    /// `"unset"`（行が無い）/ `"disabled"`（行はあるが無効）/ `"invalid"`（壊れた値）/
    /// `"default"`（間隔は運用者既定）/ `"agent"`（エージェントの値）/
    /// `"clamped"`（エージェントの値が下限未満だったので下限へ引き上げ）
    pub source: &'static str,
}

/// エージェント単位のハートビート設定を **fail-closed** に解決する。
///
/// 判定:
/// - 行が無い / 読み出しに失敗した → **無効**（`source = "unset"`）
/// - `enabled = 0` → **無効**（`source = "disabled"`）
/// - `interval_secs <= 0`（手で書き換えた等の壊れた値）→ **無効**（`source = "invalid"`）
/// - `interval_secs` が下限未満 → 下限へ**引き上げて**有効（`source = "clamped"`）
///
/// 最後だけ「無効」にせず引き上げるのは、下限を**運用者が後から上げられる**ため。
/// 運用者が下限を上げた瞬間に既存エージェントの自律実行が全部止まるのは意図と違うし、
/// 引き上げは費用・負荷が**減る**方向なので安全側に倒れる。書き込み口
/// （`set_my_heartbeat`）は逆に**拒否**する（そこには伝える相手がいるので、黙って
/// 値を変えるより「短すぎる」と返して選び直させるほうが誠実）。
pub fn resolve_agent_heartbeat(
    conn: &Connection,
    agent_id: &str,
    default_interval_secs: u64,
    min_interval_secs: u64,
) -> ResolvedAgentHeartbeat {
    // 0 秒間隔はビジーループなので、運用者が 0 を書いても 1 秒未満にはしない。
    let min = min_interval_secs.max(1);
    let fallback = default_interval_secs.max(min);

    let row = match get_agent_heartbeat_config(conn, agent_id) {
        Ok(Some(row)) => row,
        Ok(None) => {
            return ResolvedAgentHeartbeat {
                enabled: false,
                interval_secs: fallback,
                source: "unset",
            }
        }
        Err(e) => {
            // 読めない = 壊れている。有効化の方向へは倒さない。
            tracing::warn!(agent_id, "agent_heartbeat_config の読み出しに失敗: {e}");
            return ResolvedAgentHeartbeat {
                enabled: false,
                interval_secs: fallback,
                source: "unset",
            };
        }
    };

    match row.interval_secs {
        Some(v) if v <= 0 => ResolvedAgentHeartbeat {
            enabled: false,
            interval_secs: fallback,
            source: "invalid",
        },
        Some(v) => {
            let v = v as u64;
            let (interval, source) = if v < min {
                (min, "clamped")
            } else {
                (v, "agent")
            };
            ResolvedAgentHeartbeat {
                enabled: row.enabled,
                interval_secs: interval,
                source: if row.enabled { source } else { "disabled" },
            }
        }
        None => ResolvedAgentHeartbeat {
            enabled: row.enabled,
            interval_secs: fallback,
            source: if row.enabled { "default" } else { "disabled" },
        },
    }
}

/// `agent_heartbeat_config` で **enabled = 1** のエージェント id を列挙する（#238）。
///
/// 発火ループの起動対象を「エージェント単位ハートビートに opt-in 済みのエージェント」
/// へ広げるために使う。ここでは `enabled` 行を素直に返すだけで、間隔の妥当性
/// （壊れた値・下限クランプ）判定は発火時の [`resolve_agent_heartbeat`] が握る
/// （二段構え）。`interval_secs` が壊れていても「ループは起動し、発火は fail-closed で
/// 止まる」ので、ここで弾く必要はない。
pub fn list_agents_with_heartbeat_enabled(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id FROM agent_heartbeat_config WHERE enabled = 1 ORDER BY agent_id",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
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
