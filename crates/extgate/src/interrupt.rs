//! D-1096: `interrupt` — stop a session's running turn and record how much of the agent's last
//! utterance reached the other side.
//!
//! A said turn registers itself in [`RunningTurns`] while it runs. `interrupt` aborts the turn's
//! engine task; the turn then emits no say or partial of its own, closes the tools it left
//! running as `stopped`, records the `delivery_cut` note and ends its activity, all before it
//! releases the session, so the next queued turn starts at once and sees the note. Subtasks the
//! turn started are separate runs and keep running.
//!
//! With no running turn the reply has already been recorded; the note is added after it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use opencrab_actions::{ToolEvent, ToolEventSink, ToolEventStatus};
use opencrab_db::queries::{insert_session_log, insert_tool_log, SessionLogRow, ToolLogWrite};
use tokio::sync::watch;
use tokio::task::AbortHandle;

use crate::registry::ExtgateState;

/// The running said turn of each session.
#[derive(Default)]
pub struct RunningTurns {
    turns: Mutex<HashMap<String, Arc<RunningTurn>>>,
    next_serial: AtomicU64,
}

impl RunningTurns {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the turn that now holds the session.
    pub(crate) fn begin(&self, session_id: &str, agent_id: &str) -> Arc<RunningTurn> {
        let (finished, _) = watch::channel(false);
        let turn = Arc::new(RunningTurn {
            serial: self.next_serial.fetch_add(1, Ordering::Relaxed),
            agent_id: agent_id.to_string(),
            state: Mutex::new(TurnState::default()),
            finished,
            tools: Mutex::new(HashMap::new()),
        });
        self.lock()
            .insert(session_id.to_string(), Arc::clone(&turn));
        turn
    }

    /// The turn has released the session.
    pub(crate) fn end(&self, session_id: &str, turn: &RunningTurn) {
        let mut turns = self.lock();
        if turns
            .get(session_id)
            .is_some_and(|t| t.serial == turn.serial)
        {
            turns.remove(session_id);
        }
        drop(turns);
        turn.finished.send_replace(true);
    }

    pub(crate) fn get(&self, session_id: &str) -> Option<Arc<RunningTurn>> {
        self.lock().get(session_id).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<RunningTurn>>> {
        self.turns.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Default)]
struct TurnState {
    task: Option<AbortHandle>,
    /// `Some(delivered)` once interrupted.
    interrupt: Option<Option<String>>,
    /// The turn has decided its outcome; a later interrupt no longer stops it.
    closed: bool,
}

struct InFlightTool {
    name: String,
    args_json: String,
    started_at: String,
    started: Instant,
}

pub(crate) struct RunningTurn {
    serial: u64,
    agent_id: String,
    state: Mutex<TurnState>,
    finished: watch::Sender<bool>,
    tools: Mutex<HashMap<String, InFlightTool>>,
}

impl RunningTurn {
    fn state(&self) -> std::sync::MutexGuard<'_, TurnState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The task now running the turn's engine. Aborted at once if the turn was interrupted.
    pub(crate) fn set_task(&self, task: AbortHandle) {
        let mut state = self.state();
        if state.interrupt.is_some() {
            task.abort();
        }
        state.task = Some(task);
    }

    /// Stops the turn. `false` when the turn has already decided its outcome.
    fn interrupt(&self, delivered: Option<String>) -> bool {
        let mut state = self.state();
        if state.closed {
            return false;
        }
        state.interrupt = Some(delivered);
        if let Some(task) = &state.task {
            task.abort();
        }
        true
    }

    /// The turn decides its outcome: `Some(delivered)` if it was interrupted.
    pub(crate) fn close(&self) -> Option<Option<String>> {
        let mut state = self.state();
        state.closed = true;
        state.interrupt.take()
    }

    async fn wait_finished(&self) {
        let mut finished = self.finished.subscribe();
        let _ = finished.wait_for(|done| *done).await;
    }

    /// Writes a `stopped` tool log for every tool of the turn that has not finished.
    pub(crate) fn log_stopped_tools(&self, state: &ExtgateState, session_id: &str) {
        let tools = std::mem::take(&mut *self.tools.lock().unwrap_or_else(|e| e.into_inner()));
        if tools.is_empty() {
            return;
        }
        let Ok(conn) = state.db.lock() else {
            tracing::error!(%session_id, "interrupt: db lock for stopped tool logs");
            return;
        };
        for tool in tools.into_values() {
            let write = ToolLogWrite {
                agent_id: self.agent_id.clone(),
                session_id: Some(session_id.to_string()),
                tool_name: tool.name,
                args_json: tool.args_json,
                outcome: "stopped".to_string(),
                result_text: "interrupted".to_string(),
                started_at: Some(tool.started_at),
                latency_ms: Some(tool.started.elapsed().as_millis() as i64),
                iteration: None,
            };
            if let Err(e) = insert_tool_log(&conn, &write) {
                tracing::error!(%session_id, "interrupt: stopped tool log failed: {e}");
            }
        }
    }

    /// Records the cut of an interrupted turn. `say_recorded`: the turn already sent (and
    /// recorded) a say, which stays as it is; otherwise the delivered part becomes the agent's
    /// speech. The note follows.
    pub(crate) fn record_cut(
        &self,
        state: &ExtgateState,
        session_id: &str,
        say_recorded: bool,
        delivered: Option<&str>,
    ) {
        let delivered = delivered.unwrap_or("");
        let Ok(conn) = state.db.lock() else {
            tracing::error!(%session_id, "interrupt: db lock for the delivery cut");
            return;
        };
        if !say_recorded && !delivered.is_empty() {
            let row = crate::delivery::say_speech_row(&self.agent_id, session_id, delivered);
            if let Err(e) = insert_session_log(&conn, &row) {
                tracing::error!(%session_id, "interrupt: delivered speech row failed: {e}");
            }
        }
        insert_cut_note(&conn, &self.agent_id, session_id, delivered);
    }
}

/// Wraps the turn's tool event sink to know which tools of the turn are still running.
pub(crate) struct TrackedTools {
    inner: Arc<dyn ToolEventSink>,
    turn: Arc<RunningTurn>,
}

impl TrackedTools {
    pub(crate) fn new(inner: Arc<dyn ToolEventSink>, turn: Arc<RunningTurn>) -> Self {
        Self { inner, turn }
    }
}

impl ToolEventSink for TrackedTools {
    fn on_event(&self, event: &ToolEvent<'_>) {
        // Tools of a subtask (depth > 0) belong to that run, which an interrupt does not stop.
        if event.depth == 0 {
            let mut tools = self.turn.tools.lock().unwrap_or_else(|e| e.into_inner());
            if event.status == ToolEventStatus::Started {
                tools.insert(
                    event.tool_call_id.to_string(),
                    InFlightTool {
                        name: event.tool_name.to_string(),
                        args_json: event.args.to_string(),
                        started_at: event.started_at.to_string(),
                        started: Instant::now(),
                    },
                );
            } else {
                tools.remove(event.tool_call_id);
            }
        }
        self.inner.on_event(event);
    }
}

fn insert_cut_note(conn: &rusqlite::Connection, agent_id: &str, session_id: &str, delivered: &str) {
    let row = SessionLogRow {
        id: None,
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        log_type: "system".to_string(),
        content: serde_json::json!({"type": "delivery_cut", "delivered": delivered}).to_string(),
        speaker_id: None,
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    if let Err(e) = insert_session_log(conn, &row) {
        tracing::error!(%session_id, "interrupt: delivery cut note failed: {e}");
    }
}

/// Handles one `interrupt` for `session_id` and returns once its effects are recorded.
pub(crate) async fn interrupt_session(
    state: &ExtgateState,
    session_id: &str,
    agent_id: &str,
    delivered: Option<String>,
) {
    if let Some(turn) = state.running_turns.get(session_id) {
        let stopped = turn.interrupt(delivered.clone());
        turn.wait_finished().await;
        if stopped {
            return;
        }
    }
    // No running turn (or it had already decided): its reply is recorded; add the note after it.
    let Some(delivered) = delivered else {
        return;
    };
    match state.db.lock() {
        Ok(conn) => insert_cut_note(&conn, agent_id, session_id, &delivered),
        Err(_) => tracing::error!(%session_id, "interrupt: db lock for the delivery cut"),
    }
}

/// Keeps the turn registered while it holds the session; ends the registration on drop.
pub(crate) struct TurnRegistration {
    state: Arc<ExtgateState>,
    session_id: String,
    pub(crate) turn: Arc<RunningTurn>,
}

impl TurnRegistration {
    pub(crate) fn begin(state: &Arc<ExtgateState>, session_id: &str, agent_id: &str) -> Self {
        let turn = state.running_turns.begin(session_id, agent_id);
        Self {
            state: Arc::clone(state),
            session_id: session_id.to_string(),
            turn,
        }
    }
}

impl Drop for TurnRegistration {
    fn drop(&mut self) {
        self.state.running_turns.end(&self.session_id, &self.turn);
    }
}

/// Where an interrupted turn reports its end.
pub(crate) struct InterruptedTurn<'a> {
    pub state: &'a Arc<ExtgateState>,
    pub instance_id: &'a str,
    pub binding_id: &'a str,
    pub session_id: &'a str,
    pub activity_id: &'a str,
    pub activity: Option<&'a crate::activity_stream::ActivityEmitter>,
    /// The turn already sent (and recorded) a say.
    pub say_recorded: bool,
}

/// Finishes an interrupted turn instead of delivering its result: closes its running tools as
/// `stopped`, records the cut and ends its activity (no completed target).
pub(crate) async fn finish_interrupted(
    turn: &RunningTurn,
    at: InterruptedTurn<'_>,
    delivered: Option<String>,
) {
    if let Some(activity) = at.activity {
        activity.flush().await;
    }
    turn.log_stopped_tools(at.state, at.session_id);
    turn.record_cut(
        at.state,
        at.session_id,
        at.say_recorded,
        delivered.as_deref(),
    );
    crate::listen::emit_ended_activity(
        at.state,
        at.instance_id,
        at.binding_id,
        at.activity_id,
        None,
        &[],
    )
    .await;
    tracing::info!(session_id = %at.session_id, "turn interrupted");
}
