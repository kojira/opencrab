//! Discord チャンネル設定の書き込み（載せ替え §3 / RULINGS Q12）。
//!
//! 読み書き・whitelist の永続化は判断。ゲートの `execute` はここへ委譲するだけ。
//! Discord API は叩かない。露出（definitions）は Discord ゲートに残す
//! （web / Nostr へは出さない）。

use opencrab_gateway::GatewayActionResult;
use serde_json::json;

/// `discord_channel_config` の DB 書き込み。
///
/// 引数検査・省略時 patch（#421）・応答 JSON は移設前と同一。
pub fn apply_discord_channel_config(
    db: &opencrab_db::Db,
    args: &serde_json::Value,
    agent_id: &str,
) -> GatewayActionResult {
    let channel_id = match args.get("channel_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("channel_idパラメータが必要です".to_string()),
            }
        }
    };
    let guild_id = match args.get("guild_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("guild_idパラメータが必要です".to_string()),
            }
        }
    };
    let channel_name = args
        .get("channel_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let readable = match args.get("readable").and_then(|v| v.as_bool()) {
        Some(r) => r,
        None => {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("readableパラメータが必要です".to_string()),
            }
        }
    };
    let writable = match args.get("writable").and_then(|v| v.as_bool()) {
        Some(w) => w,
        None => {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("writableパラメータが必要です".to_string()),
            }
        }
    };

    // #421: whitelisted / heartbeat_enabled は省略可。full-replace で既定値へ落とすと
    // 「読み書きだけ変えたい」操作が既存の whitelist / heartbeat 設定を黙って壊す。
    let whitelisted_arg = args.get("whitelisted").and_then(|v| v.as_bool());
    let heartbeat_enabled_arg = args.get("heartbeat_enabled").and_then(|v| v.as_bool());

    let (result, whitelisted) = {
        let conn = db.lock().unwrap();
        let existing =
            opencrab_db::queries::get_channel_config_for_agent(&conn, channel_id, agent_id)
                .ok()
                .flatten();
        let whitelisted = whitelisted_arg.unwrap_or_else(|| {
            opencrab_db::queries::is_channel_whitelisted_for_agent(&conn, channel_id, agent_id)
        });
        let heartbeat_enabled = heartbeat_enabled_arg
            .or_else(|| existing.as_ref().map(|c| c.heartbeat_enabled))
            .unwrap_or(true);
        let cfg = opencrab_db::queries::ChannelConfigRow {
            channel_id: channel_id.to_string(),
            agent_id: agent_id.to_string(),
            guild_id: guild_id.to_string(),
            channel_name: channel_name.to_string(),
            readable,
            writable,
            whitelisted,
            heartbeat_enabled,
            heartbeat_interval_secs: existing.as_ref().and_then(|c| c.heartbeat_interval_secs),
            heartbeat_instructions: existing
                .map(|c| c.heartbeat_instructions)
                .unwrap_or_default(),
        };
        (
            opencrab_db::queries::upsert_channel_config(&conn, &cfg),
            whitelisted,
        )
    };

    match result {
        Ok(()) => GatewayActionResult {
            success: true,
            data: Some(json!({
                "channel_id": channel_id,
                "channel_name": channel_name,
                "readable": readable,
                "writable": writable,
                "whitelisted": whitelisted,
                "message": format!(
                    "チャンネル {} の設定を更新しました (readable={}, writable={})",
                    if channel_name.is_empty() { channel_id } else { channel_name },
                    readable,
                    writable,
                ),
            })),
            error: None,
        },
        Err(e) => GatewayActionResult {
            success: false,
            data: None,
            error: Some(format!("チャンネル設定の保存に失敗: {e}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn upsert_writes_readable_writable() {
        let db = opencrab_db::Db::memory().unwrap();
        let result = apply_discord_channel_config(
            &db,
            &json!({
                "channel_id": "ch-1",
                "guild_id": "guild-1",
                "channel_name": "general",
                "readable": true,
                "writable": false,
            }),
            "test-agent",
        );
        assert!(result.success);
        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config_for_agent(&conn, "ch-1", "test-agent")
            .unwrap()
            .unwrap();
        assert!(cfg.readable);
        assert!(!cfg.writable);
        assert_eq!(cfg.channel_name, "general");
        assert_eq!(cfg.guild_id, "guild-1");
    }

    #[test]
    fn omitted_whitelisted_preserves_existing() {
        let db = opencrab_db::Db::memory().unwrap();
        apply_discord_channel_config(
            &db,
            &json!({
                "channel_id": "ch-1",
                "guild_id": "guild-1",
                "readable": true,
                "writable": true,
                "whitelisted": true,
                "heartbeat_enabled": false,
            }),
            "test-agent",
        );
        let r2 = apply_discord_channel_config(
            &db,
            &json!({
                "channel_id": "ch-1",
                "guild_id": "guild-1",
                "readable": false,
                "writable": false,
            }),
            "test-agent",
        );
        assert!(r2.success);
        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config_for_agent(&conn, "ch-1", "test-agent")
            .unwrap()
            .unwrap();
        assert!(!cfg.readable);
        assert!(!cfg.writable);
        assert!(cfg.whitelisted);
        assert!(!cfg.heartbeat_enabled);
    }

    #[test]
    fn missing_guild_id_fails() {
        let db = opencrab_db::Db::memory().unwrap();
        let result =
            apply_discord_channel_config(&db, &json!({"channel_id": "ch-1"}), "test-agent");
        assert!(!result.success);
        assert!(result.error.unwrap().contains("guild_id"));
    }
}
