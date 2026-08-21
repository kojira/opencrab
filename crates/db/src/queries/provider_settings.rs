use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// LLM プロバイダー設定のオーバーライド（ダッシュボード編集用）
//
// TOML（[llm.providers.*]）を土台に、ここに行があるフィールドだけ上書きする。
// api_key の平文保存は agent_discord_config.bot_token と同じ扱い
// （SQLite ファイルの読み取り権限で保護。API 応答では必ずマスクする）。
// ============================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmProviderOverrideRow {
    pub provider: String,
    /// None = TOML に従う / Some(false) = 強制無効 / Some(true) = 有効
    pub enabled: Option<bool>,
    /// None = TOML/env のキーを使う。Some は空文字を許さない（clear は None で表現）。
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub default_model: Option<String>,
    /// 推論（thinking）強度。None = TOML/モデル既定。"low"|"medium"|"high"|"xhigh" 等。
    pub reasoning_effort: Option<String>,
    /// 起動バイナリ（codex/cursor/acp 等の subprocess プロバイダ）。None = TOML。
    pub binary_path: Option<String>,
    /// 起動引数の JSON 配列（acp 等）。None = TOML。
    pub args_json: Option<String>,
    /// 作業ディレクトリ。None = TOML。
    pub working_dir: Option<String>,
    /// タイムアウト秒。None = TOML。
    pub timeout_secs: Option<i64>,
}

pub fn upsert_llm_provider_override(conn: &Connection, row: &LlmProviderOverrideRow) -> Result<()> {
    conn.execute(
        "INSERT INTO llm_provider_overrides
            (provider, enabled, api_key, base_url, default_model, reasoning_effort,
             binary_path, args_json, working_dir, timeout_secs, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(provider) DO UPDATE SET
            enabled = excluded.enabled,
            api_key = excluded.api_key,
            base_url = excluded.base_url,
            default_model = excluded.default_model,
            reasoning_effort = excluded.reasoning_effort,
            binary_path = excluded.binary_path,
            args_json = excluded.args_json,
            working_dir = excluded.working_dir,
            timeout_secs = excluded.timeout_secs,
            updated_at = excluded.updated_at",
        params![
            row.provider,
            row.enabled,
            row.api_key,
            row.base_url,
            row.default_model,
            row.reasoning_effort,
            row.binary_path,
            row.args_json,
            row.working_dir,
            row.timeout_secs,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_llm_provider_override(
    conn: &Connection,
    provider: &str,
) -> Result<Option<LlmProviderOverrideRow>> {
    let result = conn.query_row(
        "SELECT provider, enabled, api_key, base_url, default_model, reasoning_effort,
                binary_path, args_json, working_dir, timeout_secs
         FROM llm_provider_overrides WHERE provider = ?1",
        params![provider],
        map_override_row,
    );
    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn list_llm_provider_overrides(conn: &Connection) -> Result<Vec<LlmProviderOverrideRow>> {
    let mut stmt = conn.prepare(
        "SELECT provider, enabled, api_key, base_url, default_model, reasoning_effort,
                binary_path, args_json, working_dir, timeout_secs
         FROM llm_provider_overrides ORDER BY provider",
    )?;
    let rows = stmt
        .query_map([], map_override_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn delete_llm_provider_override(conn: &Connection, provider: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM llm_provider_overrides WHERE provider = ?1",
        params![provider],
    )?;
    Ok(())
}

fn map_override_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LlmProviderOverrideRow> {
    Ok(LlmProviderOverrideRow {
        provider: row.get(0)?,
        enabled: row.get(1)?,
        api_key: row.get(2)?,
        base_url: row.get(3)?,
        default_model: row.get(4)?,
        reasoning_effort: row.get(5)?,
        binary_path: row.get(6)?,
        args_json: row.get(7)?,
        working_dir: row.get(8)?,
        timeout_secs: row.get(9)?,
    })
}

// ============================================
// 音声（VC）設定のオーバーライド。VoiceConfig の JSON を 1 行で保持する。
// フィールド粒度のマージはサーバ側（TOML とのマージ関数）で行う。
// ============================================

pub fn set_voice_config_override(conn: &Connection, config_json: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO voice_config_override (id, config_json, updated_at)
         VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET
            config_json = excluded.config_json,
            updated_at = excluded.updated_at",
        params![config_json, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

pub fn get_voice_config_override(conn: &Connection) -> Result<Option<String>> {
    let result = conn.query_row(
        "SELECT config_json FROM voice_config_override WHERE id = 1",
        [],
        |row| row.get::<_, String>(0),
    );
    match result {
        Ok(json) => Ok(Some(json)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete_voice_config_override(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM voice_config_override WHERE id = 1", [])?;
    Ok(())
}
