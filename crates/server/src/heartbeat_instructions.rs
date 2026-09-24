//! Gateway-neutral heartbeat-instruction tools.
//!
//! Agent scope updates the core-owned agent fallback. Session scope updates only the generic
//! `(agent_id, session_id)` override projected/provisioned for an existing heartbeat target. No
//! external destination or concrete gateway vocabulary is accepted here.

use serde_json::json;

use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};

use crate::AppState;

pub(crate) fn update_heartbeat_instructions(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    if !ctx.caller.is_owner_equivalent() {
        return failure("このアクションはオーナーのみ実行できます");
    }
    let scope = args
        .get("scope")
        .and_then(|value| value.as_str())
        .unwrap_or("agent");
    let raw = match args.get("instructions").and_then(|value| value.as_str()) {
        Some(value) => value,
        None => return failure("instructionsパラメータが必要です"),
    };
    if raw.chars().count() > opencrab_db::queries::MAX_HEARTBEAT_INSTRUCTIONS_LEN {
        return failure(&format!(
            "instructionsが長すぎます（最大{}文字）",
            opencrab_db::queries::MAX_HEARTBEAT_INSTRUCTIONS_LEN
        ));
    }
    let instructions = opencrab_db::queries::sanitize_heartbeat_instructions(raw);
    let reason = args.get("reason").and_then(|value| value.as_str());
    let conn = state.db.lock().unwrap();

    match scope {
        "agent" => {
            let old_value = opencrab_db::queries::get_agent(&conn, &ctx.agent_id)
                .ok()
                .flatten()
                .map(|agent| agent.heartbeat_instructions);
            let patch = opencrab_db::queries::AgentPatch {
                heartbeat_instructions: Some(instructions.clone()),
                ..Default::default()
            };
            match opencrab_db::queries::apply_agent_patch(&conn, &ctx.agent_id, &patch) {
                Ok(true) => {}
                Ok(false) => return failure("エージェントが見つかりません"),
                Err(error) => return failure(&format!("ハートビート指示の保存に失敗: {error}")),
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
        "session" => {
            let session_id = match args.get("session_id").and_then(|value| value.as_str()) {
                Some(value) if !value.is_empty() => value,
                _ => return failure("scope=sessionのときはsession_idが必要です"),
            };
            let old_value = opencrab_db::queries::get_session_heartbeat_instructions(
                &conn,
                &ctx.agent_id,
                session_id,
            )
            .ok()
            .flatten()
            .and_then(|row| row.override_text);
            let row = opencrab_db::queries::SessionHeartbeatInstructionsRow {
                agent_id: ctx.agent_id.clone(),
                session_id: session_id.to_string(),
                override_text: Some(instructions.clone()),
            };
            if let Err(error) =
                opencrab_db::queries::upsert_session_heartbeat_instructions(&conn, &row)
            {
                return failure(&format!("セッション指示の保存に失敗: {error}"));
            }
            record_audit(
                &conn,
                &ctx.agent_id,
                "session",
                Some(session_id),
                ctx.caller.label(),
                old_value.as_deref(),
                &instructions,
                reason,
            );
            success_response("session", Some(session_id), &instructions)
        }
        other => failure(&format!("不明なscope: {other}（agent または session）")),
    }
}

pub(crate) fn read_heartbeat_instructions(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    if !matches!(
        ctx.caller,
        GatewayCaller::Owner | GatewayCaller::CoAgent { .. } | GatewayCaller::TrustedUser
    ) {
        return failure("このアクションは信頼済みの呼び出し元のみ実行できます");
    }
    let scope = args
        .get("scope")
        .and_then(|value| value.as_str())
        .unwrap_or("effective");
    let conn = state.db.lock().unwrap();
    match scope {
        "agent" => {
            let text = opencrab_db::queries::get_agent(&conn, &ctx.agent_id)
                .ok()
                .flatten()
                .map(|agent| agent.heartbeat_instructions)
                .unwrap_or_default();
            GatewayActionResult {
                success: true,
                data: Some(json!({"scope": "agent", "instructions": text})),
                error: None,
            }
        }
        "session" | "effective" => {
            let session_id = match args.get("session_id").and_then(|value| value.as_str()) {
                Some(value) if !value.is_empty() => value,
                _ => return failure(&format!("scope={scope}のときはsession_idが必要です")),
            };
            if scope == "session" {
                let text = opencrab_db::queries::get_session_heartbeat_instructions(
                    &conn,
                    &ctx.agent_id,
                    session_id,
                )
                .ok()
                .flatten()
                .and_then(|row| row.override_text)
                .unwrap_or_default();
                GatewayActionResult {
                    success: true,
                    data: Some(json!({
                        "scope": "session",
                        "session_id": session_id,
                        "instructions": text,
                    })),
                    error: None,
                }
            } else {
                match opencrab_db::queries::resolve_session_heartbeat_instructions(
                    &conn,
                    &ctx.agent_id,
                    session_id,
                ) {
                    Ok(resolved) => GatewayActionResult {
                        success: true,
                        data: Some(json!({
                            "scope": "effective",
                            "session_id": session_id,
                            "source": resolved.source,
                            "instructions": resolved.text,
                        })),
                        error: None,
                    },
                    Err(error) => failure(&format!("ハートビート指示の読み出しに失敗: {error}")),
                }
            }
        }
        other => failure(&format!(
            "不明なscope: {other}（agent / session / effective）"
        )),
    }
}

fn failure(message: &str) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(message.to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
fn record_audit(
    conn: &rusqlite::Connection,
    agent_id: &str,
    scope: &str,
    session_id: Option<&str>,
    caller: &str,
    old_value: Option<&str>,
    new_value: &str,
    reason: Option<&str>,
) {
    let audit = opencrab_db::queries::HeartbeatInstructionsAuditRow {
        agent_id: agent_id.to_string(),
        scope: scope.to_string(),
        session_id: session_id.map(str::to_owned),
        caller_identity: caller.to_string(),
        caller_user_id: None,
        old_value: old_value.map(str::to_owned),
        new_value: Some(new_value.to_string()),
        reason: reason.map(str::to_owned),
    };
    if let Err(error) = opencrab_db::queries::insert_heartbeat_instructions_audit(conn, &audit) {
        tracing::error!(%error, "heartbeat instruction audit failed");
    }
}

fn success_response(
    scope: &str,
    session_id: Option<&str>,
    instructions: &str,
) -> GatewayActionResult {
    let preview: String = instructions.chars().take(120).collect();
    GatewayActionResult {
        success: true,
        data: Some(json!({
            "success": true,
            "scope": scope,
            "session_id": session_id,
            "length": instructions.chars().count(),
            "preview": preview,
        })),
        error: None,
    }
}
