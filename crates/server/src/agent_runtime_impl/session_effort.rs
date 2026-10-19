//! D-1086: session reasoning effort administration (the `ModelAdministration` effort commands).
//!
//! A slot stores one of [`REASONING_EFFORTS`]. The value applies from the session's next turn;
//! the session lock is never taken.

use opencrab_actions::{
    EffortSnapshot, ModelAdminError, SessionEffortList, TurnModelProfile, REASONING_EFFORTS,
};
use opencrab_db::queries::SessionEffortSlot;

use super::session_model_target;
use crate::AppState;

fn slot(profile: TurnModelProfile) -> SessionEffortSlot {
    match profile {
        TurnModelProfile::Default => SessionEffortSlot::Default,
        TurnModelProfile::Realtime => SessionEffortSlot::Realtime,
    }
}

fn snapshot(
    conn: &rusqlite::Connection,
    agent_id: &str,
    session_id: &str,
    slot: SessionEffortSlot,
) -> Result<EffortSnapshot, ModelAdminError> {
    let configured_effort =
        opencrab_db::queries::get_session_reasoning_effort(conn, session_id, slot)
            .map_err(|_| ModelAdminError::Internal)?;
    let (current_effort, source) = opencrab_db::queries::effective_reasoning_effort_for_session(
        conn, agent_id, session_id, slot,
    )
    .map_err(|_| ModelAdminError::Internal)?;
    Ok(EffortSnapshot {
        configured_effort,
        current_effort,
        source,
    })
}

pub(super) fn list(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
) -> Result<SessionEffortList, ModelAdminError> {
    let conn = state.db.lock().map_err(|_| ModelAdminError::Internal)?;
    session_model_target(&conn, agent_id, session_id)?;
    Ok(SessionEffortList {
        default: snapshot(&conn, agent_id, session_id, SessionEffortSlot::Default)?,
        realtime: snapshot(&conn, agent_id, session_id, SessionEffortSlot::Realtime)?,
    })
}

/// Writes `effort` (`None` = reset) into the `profile` slot after the target check.
pub(super) fn write(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
    profile: TurnModelProfile,
    effort: Option<&str>,
) -> Result<EffortSnapshot, ModelAdminError> {
    if effort.is_some_and(|effort| !REASONING_EFFORTS.contains(&effort)) {
        return Err(ModelAdminError::InvalidArgs);
    }
    let conn = state.db.lock().map_err(|_| ModelAdminError::Internal)?;
    session_model_target(&conn, agent_id, session_id)?;
    let slot = slot(profile);
    if !opencrab_db::queries::set_session_reasoning_effort(&conn, session_id, slot, effort)
        .map_err(|_| ModelAdminError::Internal)?
    {
        return Err(ModelAdminError::TargetUnavailable);
    }
    snapshot(&conn, agent_id, session_id, slot)
}
