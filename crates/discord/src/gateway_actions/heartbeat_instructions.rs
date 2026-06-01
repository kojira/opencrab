//! ハートビート指示の更新・読み出しゲートウェイアクション。
//!
//! - `update_heartbeat_instructions`: Owner限定。`agents.heartbeat_instructions` または
//!   `discord_channel_config.heartbeat_instructions` を更新し、監査ログを残す。
//! - `read_heartbeat_instructions`: trusted限定。agent / channel / effective を読み出す。
//!
//! 権限境界は [`crate::gateway_actions`] のディスパッチに加え、`bridge.rs` のフィルタ
//! （owner_only_actions / trusted_only_actions）でも強制される（多層防御）。

use opencrab_gateway::GatewayActionResult;
use serde_json::json;

use super::DiscordGatewayActions;

impl DiscordGatewayActions {
    pub(crate) fn execute_update_heartbeat_instructions(
        &self,
        args: &serde_json::Value,
    ) -> GatewayActionResult {
        let caller = args
            .get("__caller")
            .and_then(|v| v.as_str())
            .unwrap_or("agent");
        if caller != "owner" {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("このアクションはオーナーのみ実行できます".to_string()),
            };
        }

        let scope = args
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("agent");
        let raw_instructions = match args.get("instructions").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("instructionsパラメータが必要です".to_string()),
                }
            }
        };

        if raw_instructions.chars().count() > opencrab_db::queries::MAX_HEARTBEAT_INSTRUCTIONS_LEN {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!(
                    "instructionsが長すぎます（最大{}文字）",
                    opencrab_db::queries::MAX_HEARTBEAT_INSTRUCTIONS_LEN
                )),
            };
        }
        let instructions = opencrab_db::queries::sanitize_heartbeat_instructions(raw_instructions);
        let reason = args.get("reason").and_then(|v| v.as_str());
        // bridge.rs のenrichmentパターンに従い `__caller_discord_id` を読む。
        // ActionContext に discord_id フィールドが無い現状ではbridgeはこれを注入しないため、
        // 通常はNoneになる（外部から明示指定された場合のみ記録される）。
        let caller_discord_id = args.get("__caller_discord_id").and_then(|v| v.as_str());

        let conn = self.db.lock().unwrap();

        match scope {
            "agent" => {
                let old_value = opencrab_db::queries::get_agent(&conn, &self.agent_id)
                    .ok()
                    .flatten()
                    .map(|a| a.heartbeat_instructions);
                let patch = opencrab_db::queries::AgentPatch {
                    heartbeat_instructions: Some(instructions.clone()),
                    ..Default::default()
                };
                match opencrab_db::queries::apply_agent_patch(&conn, &self.agent_id, &patch) {
                    Ok(true) => {}
                    Ok(false) => {
                        return GatewayActionResult {
                            success: false,
                            data: None,
                            error: Some("エージェントが見つかりません".to_string()),
                        }
                    }
                    Err(e) => {
                        return GatewayActionResult {
                            success: false,
                            data: None,
                            error: Some(format!("ハートビート指示の保存に失敗: {e}")),
                        }
                    }
                }
                self.record_audit(
                    &conn,
                    "agent",
                    None,
                    caller,
                    caller_discord_id,
                    old_value.as_deref(),
                    &instructions,
                    reason,
                );
                Self::success_response("agent", None, &instructions)
            }
            "channel" => {
                let channel_id = match args.get("channel_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.is_empty() => id,
                    _ => {
                        return GatewayActionResult {
                            success: false,
                            data: None,
                            error: Some("scope=channelのときはchannel_idが必要です".to_string()),
                        }
                    }
                };
                let existing = opencrab_db::queries::get_channel_config_for_agent(
                    &conn,
                    channel_id,
                    &self.agent_id,
                )
                .ok()
                .flatten();
                let old_value = existing.as_ref().map(|c| c.heartbeat_instructions.clone());

                // 既存行があればその設定を尊重し、なければ既定値で新規作成する。
                let guild_id = args
                    .get("guild_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| existing.as_ref().map(|c| c.guild_id.clone()));
                let guild_id = match guild_id {
                    Some(g) if !g.is_empty() => g,
                    _ => {
                        return GatewayActionResult {
                            success: false,
                            data: None,
                            error: Some(
                                "新規チャンネル設定の作成にはguild_idが必要です".to_string(),
                            ),
                        }
                    }
                };

                let cfg = match existing {
                    Some(mut c) => {
                        c.heartbeat_instructions = instructions.clone();
                        c
                    }
                    None => opencrab_db::queries::ChannelConfigRow {
                        channel_id: channel_id.to_string(),
                        agent_id: self.agent_id.clone(),
                        guild_id,
                        channel_name: String::new(),
                        readable: true,
                        writable: true,
                        whitelisted: false,
                        heartbeat_enabled: true,
                        heartbeat_interval_secs: None,
                        heartbeat_instructions: instructions.clone(),
                    },
                };

                if let Err(e) = opencrab_db::queries::upsert_channel_config(&conn, &cfg) {
                    return GatewayActionResult {
                        success: false,
                        data: None,
                        error: Some(format!("チャンネル指示の保存に失敗: {e}")),
                    };
                }
                self.record_audit(
                    &conn,
                    "channel",
                    Some(channel_id),
                    caller,
                    caller_discord_id,
                    old_value.as_deref(),
                    &instructions,
                    reason,
                );
                Self::success_response("channel", Some(channel_id), &instructions)
            }
            other => GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("不明なscope: {other}（agent または channel）")),
            },
        }
    }

    pub(crate) fn execute_read_heartbeat_instructions(
        &self,
        args: &serde_json::Value,
    ) -> GatewayActionResult {
        // 実行時にも呼び出し元権限を強制する（多層防御）。
        // owner / trusted_user / co_agent は許可、素のagentは拒否。
        let caller = args
            .get("__caller")
            .and_then(|v| v.as_str())
            .unwrap_or("agent");
        if !matches!(caller, "owner" | "trusted_user" | "co_agent") {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("このアクションは信頼済みの呼び出し元のみ実行できます".to_string()),
            };
        }

        let scope = args
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("effective");
        let conn = self.db.lock().unwrap();

        match scope {
            "agent" => {
                let text = opencrab_db::queries::get_agent(&conn, &self.agent_id)
                    .ok()
                    .flatten()
                    .map(|a| a.heartbeat_instructions)
                    .unwrap_or_default();
                GatewayActionResult {
                    success: true,
                    data: Some(json!({ "scope": "agent", "instructions": text })),
                    error: None,
                }
            }
            "channel" | "effective" => {
                let channel_id = match args.get("channel_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.is_empty() => id,
                    _ => {
                        return GatewayActionResult {
                            success: false,
                            data: None,
                            error: Some(format!("scope={scope}のときはchannel_idが必要です")),
                        }
                    }
                };
                if scope == "channel" {
                    let text = opencrab_db::queries::get_channel_config_for_agent(
                        &conn,
                        channel_id,
                        &self.agent_id,
                    )
                    .ok()
                    .flatten()
                    .map(|c| c.heartbeat_instructions)
                    .unwrap_or_default();
                    GatewayActionResult {
                        success: true,
                        data: Some(
                            json!({ "scope": "channel", "channel_id": channel_id, "instructions": text }),
                        ),
                        error: None,
                    }
                } else {
                    let resolved = opencrab_db::queries::resolve_heartbeat_instructions(
                        &conn,
                        &self.agent_id,
                        channel_id,
                    );
                    GatewayActionResult {
                        success: true,
                        data: Some(json!({
                            "scope": "effective",
                            "channel_id": channel_id,
                            "source": resolved.source,
                            "instructions": resolved.text,
                        })),
                        error: None,
                    }
                }
            }
            other => GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!(
                    "不明なscope: {other}（agent / channel / effective）"
                )),
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record_audit(
        &self,
        conn: &rusqlite::Connection,
        scope: &str,
        channel_id: Option<&str>,
        caller: &str,
        caller_discord_id: Option<&str>,
        old_value: Option<&str>,
        new_value: &str,
        reason: Option<&str>,
    ) {
        let audit = opencrab_db::queries::HeartbeatInstructionsAuditRow {
            agent_id: self.agent_id.clone(),
            scope: scope.to_string(),
            channel_id: channel_id.map(|s| s.to_string()),
            caller_identity: caller.to_string(),
            caller_discord_id: caller_discord_id.map(|s| s.to_string()),
            old_value: old_value.map(|s| s.to_string()),
            new_value: Some(new_value.to_string()),
            reason: reason.map(|s| s.to_string()),
        };
        if let Err(e) = opencrab_db::queries::insert_heartbeat_instructions_audit(conn, &audit) {
            tracing::error!("Failed to record heartbeat instructions audit: {e}");
        }
    }

    fn success_response(
        scope: &str,
        channel_id: Option<&str>,
        instructions: &str,
    ) -> GatewayActionResult {
        let preview: String = instructions.chars().take(120).collect();
        GatewayActionResult {
            success: true,
            data: Some(json!({
                "success": true,
                "scope": scope,
                "channel_id": channel_id,
                "length": instructions.chars().count(),
                "preview": preview,
            })),
            error: None,
        }
    }
}
