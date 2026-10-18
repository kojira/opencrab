//! D-1085: starting a provisional run from a `provisional` frame.

use std::sync::Arc;

use opencrab_actions::{run_provisional_turn, AgentRuntime, RunRequest};
use opencrab_db::queries::SessionLogRow;

use super::{ProvisionalInputs, PROVISIONAL_TTL};
use crate::error::{ErrorCode, GateError};
use crate::inbound::{
    agent_prelude_row, asserted_caller, binding_said_error, channel_whitelisted,
    inbound_speech_metadata, load_origin_row,
};
use crate::partial_stream::{partial_text_hook, PartialStreamSlot};
use crate::protocol::Provisional;
use crate::registry::ExtgateState;

/// Starts the provisional run without the session lock and registers it. Reads the session once
/// (high-water mark); writes nothing. A provisional the said would not admit starts nothing.
pub(crate) async fn start_provisional<R: AgentRuntime>(
    state: &Arc<ExtgateState>,
    runtime: &R,
    instance_id: &str,
    provisional: &Provisional,
) -> Result<(), GateError> {
    let binding_id = provisional.binding_id.as_str();
    {
        let reg = state.lock_registry()?;
        match reg.get(instance_id) {
            Some(live) if live.acknowledged.contains(binding_id) => {}
            Some(_) => {
                drop(reg);
                return binding_said_error(state, instance_id, binding_id).map(|_| ());
            }
            None => return Err(GateError::new(ErrorCode::InstanceUnknown)),
        }
    }
    let (row, session_id, high_water) = {
        let conn = state
            .db
            .lock()
            .map_err(|e| GateError::store_logged("provisional.db_lock", e))?;
        let Some(row) = load_origin_row(&conn, instance_id, binding_id)? else {
            drop(conn);
            return binding_said_error(state, instance_id, binding_id).map(|_| ());
        };
        let session_id =
            match opencrab_db::queries::canonical_session_id(&conn, binding_id, &row.address) {
                Ok(Some(id)) => id,
                Ok(None) => {
                    return Err(GateError::store_logged(
                        "provisional.session_missing",
                        format!("no session for binding={binding_id}"),
                    ))
                }
                Err(error) => {
                    return Err(GateError::store_logged("provisional.session_lookup", error))
                }
            };
        if !channel_whitelisted(&conn, &row.agent_id, instance_id, &row.address) {
            return Ok(());
        }
        let high_water = opencrab_db::queries::session_log_high_water(&conn, &session_id)
            .map_err(|e| GateError::store_logged("provisional.high_water", e))?;
        (row, session_id, high_water)
    };

    let inputs = ProvisionalInputs {
        author_id: provisional.author_id.clone(),
        author_label: provisional.author_label.clone(),
        caller: provisional.caller.clone(),
        system_context: provisional.system_context.clone().unwrap_or_default(),
        agent_prelude: provisional.agent_prelude.clone(),
        text: provisional.text.clone(),
    };
    let pending = pending_rows(&row.agent_id, &session_id, &row.address, &inputs);
    let (system, name) = runtime.build_realtime_context(&row.agent_id);
    let system = crate::completion::with_system_context(system, &inputs.system_context);
    let stream = PartialStreamSlot::new();
    let hook = partial_text_hook(
        Arc::clone(state),
        instance_id.to_string(),
        binding_id.to_string(),
        stream.clone(),
        Some(provisional.provisional_id.clone()),
    );
    let caller = asserted_caller(&inputs.caller);
    let task = {
        let runtime = runtime.clone();
        let session_id = session_id.clone();
        let agent_id = row.agent_id.clone();
        tokio::spawn(async move {
            run_provisional_turn(
                &runtime,
                &session_id,
                &agent_id,
                &system,
                high_water,
                &pending,
                |conversation| {
                    RunRequest::new(
                        agent_id.clone(),
                        name.clone(),
                        session_id.clone(),
                        system.clone(),
                        conversation,
                        "extgate",
                        caller.clone(),
                    )
                    .with_on_partial_text(hook)
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
    address: &str,
    inputs: &ProvisionalInputs,
) -> Vec<SessionLogRow> {
    let now = chrono::Utc::now().to_rfc3339();
    let meta = inbound_speech_metadata(address, inputs.author_label.as_deref(), &inputs.caller);
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
