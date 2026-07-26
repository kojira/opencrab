//! ハートビート指示ツールの gateway 非依存実装（#157 S3）。
//!
//! - `update_heartbeat_instructions`: Owner 限定。`agents.heartbeat_instructions` または
//!   チャンネル設定テーブルの `heartbeat_instructions` を更新し、監査ログを残す。
//! - `read_heartbeat_instructions`: trusted 限定。agent / channel / effective を読み出す。
//!
//! 旧実装は Discord ゲートウェイ（`crates/discord` の
//! `gateway_actions/heartbeat_instructions.rs`）にあり、**serenity を一切参照していない**
//! （依存は DB だけ）のに Discord 経由のターンでしか露出しなかった（#157 / #155）。
//! そのまま `SystemGatewayActions` の own ツールへ移し、web / Nostr / REST / heartbeat の
//! 全ターンで使えるようにする。
//!
//! 移設で維持している不変条件（順に対応するテストがある）:
//! - **レスポンス JSON のキーと文言**（エラー文言も含む）を旧実装と 1 文字も変えない。
//! - **ハンドラ内の権限検査**（update = owner のみ / read = owner・co_agent・trusted_user）。
//!   bridge の `OWNER_ONLY_ACTIONS` / `TRUSTED_ONLY_ACTIONS` も同じ名前で可視性と実行を
//!   ゲートするが（多層防御）、ここの検査も残す（fail-closed）。
//! - **監査ログ**（`heartbeat_instructions_audit` への old/new/reason 記録）。
//! - 長さ上限と sanitize（`MAX_HEARTBEAT_INSTRUCTIONS_LEN` / `sanitize_heartbeat_instructions`）。
//!
//! # チャンネル単位設定の非対称（移設で意図的に残る差）
//!
//! `scope="agent"` はエージェント行（`agents`）を読み書きするので**全経路で機能する**。
//! 一方 `scope="channel"` / `scope="effective"` が触るのは Discord のチャンネル設定
//! テーブルであり、そこに行を作るのは Discord のチャンネル運用（`discord_channel_config`
//! ツールや heartbeat ループのチャンネル列挙）だけである。したがって:
//!
//! - **Discord 運用時**: 従来どおりチャンネル上書きの参照・更新ができる。
//! - **非 Discord 経路**（web / Nostr / REST / heartbeat）: ツールは露出し実行もできるが、
//!   その `channel_id` に行が無いのが通常なので、`scope="channel"` の読み出しは
//!   **空文字列**、`scope="effective"` はエージェント/既定へのフォールバック結果を返す。
//!   更新は `guild_id` を明示すれば行を新規作成できるが、その行を消費するのは Discord
//!   ループだけなので、実質的に意味を持つのは Discord 運用時のみである。
//!
//! この非対称は**設計上の割り切り**で、移設によって生じたものではない（旧実装でも
//! チャンネル設定は Discord のものだった）。テーブル名・カラム名の Discord 依存を
//! 解消する改名は #159 の担当なので、ここでは触らない。

use serde_json::json;

use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};

use crate::AppState;

/// ハートビート指示の更新（owner 限定）。
///
/// 旧 `DiscordGatewayActions::execute_update_heartbeat_instructions` の移設。
pub(crate) fn update_heartbeat_instructions(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    if ctx.caller != GatewayCaller::Owner {
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

    let conn = state.db.lock().unwrap();

    match scope {
        "agent" => {
            let old_value = opencrab_db::queries::get_agent(&conn, &ctx.agent_id)
                .ok()
                .flatten()
                .map(|a| a.heartbeat_instructions);
            let patch = opencrab_db::queries::AgentPatch {
                heartbeat_instructions: Some(instructions.clone()),
                ..Default::default()
            };
            match opencrab_db::queries::apply_agent_patch(&conn, &ctx.agent_id, &patch) {
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
            record_audit(
                &conn,
                &ctx.agent_id,
                "agent",
                None,
                ctx.caller.label(),
                old_value.as_deref(),
                &instructions,
                reason,
            );
            success_response("agent", None, &instructions)
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
                &ctx.agent_id,
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
                        error: Some("新規チャンネル設定の作成にはguild_idが必要です".to_string()),
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
                    agent_id: ctx.agent_id.clone(),
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
            record_audit(
                &conn,
                &ctx.agent_id,
                "channel",
                Some(channel_id),
                ctx.caller.label(),
                old_value.as_deref(),
                &instructions,
                reason,
            );
            success_response("channel", Some(channel_id), &instructions)
        }
        other => GatewayActionResult {
            success: false,
            data: None,
            error: Some(format!("不明なscope: {other}（agent または channel）")),
        },
    }
}

/// ハートビート指示の読み出し（trusted 限定）。
///
/// 旧 `DiscordGatewayActions::execute_read_heartbeat_instructions` の移設。
pub(crate) fn read_heartbeat_instructions(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    // 実行時にも呼び出し元権限を強制する（多層防御）。
    // owner / trusted_user / co_agent の許可リスト（将来 variant が増えても fail-closed）。
    if !matches!(
        ctx.caller,
        GatewayCaller::Owner | GatewayCaller::CoAgent { .. } | GatewayCaller::TrustedUser
    ) {
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
    let conn = state.db.lock().unwrap();

    match scope {
        "agent" => {
            let text = opencrab_db::queries::get_agent(&conn, &ctx.agent_id)
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
                    &ctx.agent_id,
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
                    &ctx.agent_id,
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
    conn: &rusqlite::Connection,
    agent_id: &str,
    scope: &str,
    channel_id: Option<&str>,
    caller: &str,
    old_value: Option<&str>,
    new_value: &str,
    reason: Option<&str>,
) {
    let audit = opencrab_db::queries::HeartbeatInstructionsAuditRow {
        agent_id: agent_id.to_string(),
        scope: scope.to_string(),
        channel_id: channel_id.map(|s| s.to_string()),
        caller_identity: caller.to_string(),
        // bridge は Discord ユーザーIDを持たないため常に None（旧 __caller_discord_id
        // 読みは注入元が存在しない死にコードだったので削除 — #36）。
        caller_discord_id: None,
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
