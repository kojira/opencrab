//! D-1085: starting a provisional run from a `provisional` frame.

use std::sync::Arc;

use opencrab_actions::{
    run_provisional_turn, AgentRuntime, RunRequest, SubtaskCompletionSink, TurnModelProfile,
};
use opencrab_core::PromotionGate;
use opencrab_db::queries::SessionLogRow;

use super::{ProvisionalInputs, PROVISIONAL_TTL};
use crate::completion::{v3_attach_dispatch, ExtgateCompletionSink};
use crate::error::{ErrorCode, GateError};
use crate::inbound::{agent_prelude_row, asserted_caller, inbound_speech_metadata, resolve};
use crate::partial_stream::{partial_text_hook, PartialStreamSlot};
use crate::protocol::Provisional;
use crate::registry::ExtgateState;

/// Starts the provisional run without the session lock and registers it. Reads the session once
/// (high-water mark); writes nothing before its promotion. §12: the run gets the gateway operations and the dispatch sink the said's
/// normal turn would get, so its tool set is the same.
pub(crate) async fn start_provisional<R: AgentRuntime>(
    state: &Arc<ExtgateState>,
    runtime: &R,
    instance_id: &str,
    provisional: &Provisional,
) -> Result<(), GateError> {
    if !state.lock_registry()?.is_live(instance_id) {
        return Err(GateError::new(ErrorCode::InstanceUnknown));
    }
    let (row, high_water) = {
        let mut conn = state
            .db
            .lock()
            .map_err(|e| GateError::store_logged("provisional.db_lock", e))?;
        let row = resolve(&mut conn, instance_id, &provisional.session_key, None)?;
        let high_water = opencrab_db::queries::session_log_high_water(&conn, &row.session_id)
            .map_err(|e| GateError::store_logged("provisional.high_water", e))?;
        (row, high_water)
    };
    let session_id = row.session_id.clone();
    let binding_id = row.row_id.as_str();

    let inputs = ProvisionalInputs {
        author_id: provisional.author_id.clone(),
        author_label: provisional.author_label.clone(),
        caller: provisional.caller.clone(),
        system_context: provisional.system_context.clone().unwrap_or_default(),
        agent_prelude: provisional.agent_prelude.clone(),
        text: provisional.text.clone(),
    };
    let pending = pending_rows(&row.agent_id, &session_id, &row.session_key, &inputs);
    let caller = asserted_caller(&inputs.caller);
    // §13: the said's normal turn builds this system prompt for the same agent and caller.
    let (system, name) = runtime.build_agent_context(&row.agent_id, &caller);
    let system = crate::completion::with_system_context(system, &inputs.system_context);
    let stream = PartialStreamSlot::new();
    let hook = partial_text_hook(
        Arc::clone(state),
        instance_id.to_string(),
        binding_id.to_string(),
        stream.clone(),
        Some(provisional.provisional_id.clone()),
    );
    let gate = PromotionGate::new();
    // §13.2: continuation speech waits for the promotion like the run's other rows.
    let speech = super::speech::HeldSpeech::new();
    let speech_hook = speech.hook(
        state.db.clone(),
        gate.clone(),
        stream.clone(),
        row.agent_id.clone(),
        session_id.clone(),
    );
    let ops_actions: Option<Arc<dyn opencrab_gateway::GatewayActions>> =
        crate::ops_projection::ExtgateOpsGatewayActions::for_binding(
            Arc::clone(state),
            instance_id,
            binding_id,
            &session_id,
            &row.agent_id,
        )
        .map(|a| Arc::new(a) as Arc<dyn opencrab_gateway::GatewayActions>);
    let sink: Arc<dyn SubtaskCompletionSink> = Arc::new(ExtgateCompletionSink {
        state: Arc::clone(state),
        runtime: runtime.clone(),
        instance_id: instance_id.to_string(),
        binding_id: binding_id.to_string(),
        agent_id: row.agent_id.clone(),
        session_id: session_id.clone(),
        only_speaker: false,
        speaker_id: inputs.author_id.clone(),
        system_context: inputs.system_context.clone(),
        model_profile: TurnModelProfile::Realtime,
        // 先行生成は採用されるまで会話ではないので、活動は流さない。
        activity: None,
    });
    let task = {
        let runtime = runtime.clone();
        let session_id = session_id.clone();
        let agent_id = row.agent_id.clone();
        let author_id = inputs.author_id.clone();
        let gate = gate.clone();
        tokio::spawn(async move {
            let registry = runtime.subtask_registry_for(&session_id);
            run_provisional_turn(
                &runtime,
                &session_id,
                &agent_id,
                &system,
                high_water,
                &pending,
                gate,
                |conversation| {
                    let mut req = RunRequest::new(
                        agent_id.clone(),
                        name.clone(),
                        session_id.clone(),
                        system.clone(),
                        conversation,
                        "extgate",
                        caller.clone(),
                    )
                    .with_on_partial_text(hook)
                    .with_on_continuation_speech(speech_hook);
                    if let Some(ga) = ops_actions {
                        req = req.with_gateway_actions(ga);
                    }
                    v3_attach_dispatch(req, false, author_id, registry, sink)
                },
            )
            .await
        })
    };
    let serial = state.provisionals.insert(
        binding_id,
        &provisional.provisional_id,
        session_id,
        high_water,
        inputs,
        stream,
        speech,
        gate,
        task,
    );
    let reaper_state = Arc::clone(state);
    let reaper_binding = binding_id.to_string();
    let reaper_id = provisional.provisional_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(PROVISIONAL_TTL).await;
        reaper_state
            .provisionals
            .expire(&reaper_binding, &reaper_id, serial);
    });
    Ok(())
}

/// The provisional user line and agent prelude, shaped like the rows the said would record.
fn pending_rows(
    agent_id: &str,
    session_id: &str,
    session_key: &str,
    inputs: &ProvisionalInputs,
) -> Vec<SessionLogRow> {
    let now = chrono::Utc::now().to_rfc3339();
    let meta = inbound_speech_metadata(session_key, inputs.author_label.as_deref(), &inputs.caller);
    let mut rows = vec![SessionLogRow {
        id: None,
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        log_type: "speech".to_string(),
        content: inputs.text.clone(),
        speaker_id: Some(inputs.author_id.clone()),
        turn_number: None,
        metadata_json: Some(meta.to_string()),
        created_at: Some(now.clone()),
    }];
    if let Some(prelude) = &inputs.agent_prelude {
        let mut row = agent_prelude_row(session_id, agent_id, prelude);
        row.created_at = Some(now);
        rows.push(row);
    }
    rows
}
