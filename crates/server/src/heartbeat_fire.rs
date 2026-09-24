//! Generic core timed-turn dispatch through canonical binding/session IDs.

use opencrab_actions::{CallerIdentity, FireTarget};

use crate::AppState;

pub const HEARTBEAT_NEUTRAL_CHANNEL_LABEL: &str = "（この会話）";

fn format_heartbeat_prompt(channel_name: &str, instructions_text: &str) -> String {
    let action = "取り組むことがあれば、この応答はそのままセッションの gateway へ投稿されるので、これから何をするかを自分の言葉で短く添えたうえで、実作業は spawn_subtask で起動してください。";
    format!(
        "[ハートビート] 現在の会話「{channel_name}」。{instructions_text}\nいまはハートビートの時間です。{action}いま何もすることが無ければ、通常のターンと同じく NO_REPLY とだけ答えてください。"
    )
}

fn record_heartbeat_fire(db: &opencrab_db::Db, agent_id: &str, target: &FireTarget, source: &str) {
    let Ok(conn) = db.lock() else {
        return;
    };
    let result = serde_json::json!({
        "binding_id": target.binding_id,
        "session_id": target.session_id,
        "source": source,
    });
    if let Err(error) = opencrab_db::queries::insert_heartbeat_log(
        &conn,
        agent_id,
        "fired",
        Some(&result.to_string()),
    ) {
        tracing::error!(agent_id, %error, "heartbeat fire log failed");
    }
}

pub async fn run_one_heartbeat(
    state: &AppState,
    agent_id: &str,
    target: &FireTarget,
) -> Option<()> {
    let (prompt, instructions_source) = {
        let conn = state.db.lock().ok()?;
        let resolved = opencrab_db::queries::resolve_session_heartbeat_instructions(
            &conn,
            agent_id,
            &target.session_id,
        )
        .ok()?;
        (
            format_heartbeat_prompt(HEARTBEAT_NEUTRAL_CHANNEL_LABEL, &resolved.text),
            resolved.source,
        )
    };
    let Some(sink) = state.timed_fire_router.resolve() else {
        tracing::warn!(
            agent_id,
            binding_id = target.binding_id,
            session_id = target.session_id,
            "timed-fire: no live generic runtime sink"
        );
        return None;
    };
    tracing::info!(
        agent_id,
        binding_id = target.binding_id,
        session_id = target.session_id,
        prompt_preview = %opencrab_actions::prompt_preview(&prompt),
        "timed-fire: dispatch"
    );
    sink.fire_timed_turn(opencrab_actions::TimedFireRequest {
        binding_id: target.binding_id.clone(),
        session_id: target.session_id.clone(),
        agent_id: agent_id.to_string(),
        prompt,
        caller: CallerIdentity::Owner,
    });
    record_heartbeat_fire(&state.db, agent_id, target, instructions_source);
    Some(())
}
