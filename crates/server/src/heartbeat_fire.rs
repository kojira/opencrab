//! Generic core timed-turn dispatch through canonical binding/session IDs.

use opencrab_actions::{CallerIdentity, FireTarget};

use crate::AppState;

pub const HEARTBEAT_NEUTRAL_SESSION_LABEL: &str = "（この会話）";

/// 定時実行の発火を、事実だけで伝える文（D-1120）。告知するか・委譲するか・何もしないかは LLM が
/// 決める（core は行動を指示しない）。この文は会話の履歴に「発火」の出来事として 1 回だけ記録され、
/// 後続の resume ターンへ指示として再注入されない。
fn format_heartbeat_prompt(session_label: &str, instructions_text: &str) -> String {
    format!(
        "[ハートビート] この会話「{session_label}」の定時実行が発火しました。予定のメッセージ:\n{instructions_text}\nこのターンで書いた本文は、この会話の gateway へ投稿として届きます。NO_REPLY とだけ書くと、何もしません。"
    )
}

fn record_heartbeat_fire(
    db: &opencrab_db::Db,
    agent_id: &str,
    target: &FireTarget,
    schedule_id: i64,
) {
    let Ok(conn) = db.lock() else {
        return;
    };
    let result = serde_json::json!({
        "binding_id": target.binding_id,
        "session_id": target.session_id,
        "source": "schedule",
        "schedule_id": schedule_id,
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

/// `agent_schedules` の 1 行を TimedFire で発火する（#612・時間トリガーの唯一の発火経路）。
///
/// プロンプトは行の `message` をハートビートの枠で包んだもの。scheduler の定時発火と
/// `run_my_schedule` の手動発火が同じこの関数を通る。`last_fired_at` は刻まない
/// （刻むのは scheduler の発火ループだけ）。
pub async fn run_one_heartbeat(
    state: &AppState,
    agent_id: &str,
    target: &FireTarget,
    schedule_id: i64,
    message: &str,
) -> Option<()> {
    let prompt = format_heartbeat_prompt(HEARTBEAT_NEUTRAL_SESSION_LABEL, message);
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
        schedule_id,
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
    record_heartbeat_fire(&state.db, agent_id, target, schedule_id);
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D-1120: the fire states facts only; it does not tell the agent to announce or to delegate.
    #[test]
    fn heartbeat_prompt_states_facts_without_directing_behaviour() {
        let prompt =
            format_heartbeat_prompt(HEARTBEAT_NEUTRAL_SESSION_LABEL, "ニュースを1件探して投稿");
        assert!(prompt.contains("ニュースを1件探して投稿"));
        assert!(prompt.contains("NO_REPLY"));
        for directive in ["spawn_subtask", "添えたうえで", "起動してください"] {
            assert!(
                !prompt.contains(directive),
                "no core-imposed directive {directive:?}: {prompt}"
            );
        }
    }
}
