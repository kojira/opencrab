use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use opencrab_gate_client::client::{
    CreateBindingError, InstanceClient, LiveEvent, PostRefuse, SaidOutcome,
};
use opencrab_gate_client::wire::{
    config_digest, said_frame_with_context, LiveInboundScope, SaidCaller, SaidContext, MAX_FRAME,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use crate::args::SessionArg;
use crate::config::{InstancePlacement, Placement};
use crate::jsonl::{self, Event, Input, Output, ReadRecord};
use crate::repl;

const QUEUE_CAPACITY: usize = 32;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontend {
    Repl,
    Jsonl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Interrupt,
    Terminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Normal,
    Interrupted,
}

pub struct RunOptions {
    pub placement: Placement,
    pub instance: InstancePlacement,
    pub session: SessionArg,
    pub frontend: Frontend,
    pub connect_timeout: Duration,
    /// Production passes ten seconds. Tests may shorten the same bounded-drain path.
    pub terminate_drain: Duration,
}

#[derive(Clone)]
struct Emitters {
    frontend: Frontend,
    output: mpsc::Sender<Output>,
    diagnostics: mpsc::Sender<String>,
}

impl Emitters {
    async fn event(&self, event: Event) -> anyhow::Result<()> {
        if self.frontend == Frontend::Repl {
            if let Event::Error { code, .. } = &event {
                return self.diagnostic(code).await;
            }
        }
        self.output
            .send(Output::Event(event))
            .await
            .map_err(|_| anyhow::anyhow!("output_closed"))
    }

    async fn error(&self, request_id: Option<String>, code: &str) -> anyhow::Result<()> {
        self.event(error_event(request_id, code)).await
    }

    async fn diagnostic(&self, code: &str) -> anyhow::Result<()> {
        self.diagnostics
            .send(format!("error: {}", repl::safe_text(code)))
            .await
            .map_err(|_| anyhow::anyhow!("diagnostic output closed"))
    }
}

struct ConnectionState {
    ready: AtomicBool,
    transition: Mutex<()>,
    agent_id: String,
    session_id: String,
}

impl ConnectionState {
    fn new(agent_id: String, session_id: String) -> Self {
        Self {
            ready: AtomicBool::new(true),
            transition: Mutex::new(()),
            agent_id,
            session_id,
        }
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    async fn set(&self, current: bool, emitters: &Emitters) -> anyhow::Result<()> {
        let _guard = self.transition.lock().await;
        self.set_locked(current, emitters).await
    }

    async fn disconnected_request_error(
        &self,
        request_id: String,
        emitters: &Emitters,
    ) -> anyhow::Result<()> {
        let _guard = self.transition.lock().await;
        self.set_locked(false, emitters).await?;
        emitters.error(Some(request_id), "disconnect").await
    }

    async fn set_locked(&self, current: bool, emitters: &Emitters) -> anyhow::Result<()> {
        let previous = self.ready.load(Ordering::SeqCst);
        if previous == current {
            return Ok(());
        }
        self.ready.store(current, Ordering::SeqCst);
        let state = if current { "connected" } else { "disconnected" };
        emitters.event(Event::Connection { state }).await?;
        if current {
            emitters
                .event(Event::Ready {
                    agent_id: self.agent_id.clone(),
                    session_id: self.session_id.clone(),
                    state: "connected",
                })
                .await?;
        }
        Ok(())
    }
}

pub async fn run<R, W, E>(
    options: RunOptions,
    stdin: R,
    stdout: W,
    stderr: E,
    mut controls: mpsc::Receiver<Control>,
) -> anyhow::Result<Exit>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    E: AsyncWrite + Unpin + Send + 'static,
{
    let (output_tx, output_rx) = mpsc::channel(QUEUE_CAPACITY);
    let (diagnostic_tx, diagnostic_rx) = mpsc::channel(QUEUE_CAPACITY);
    let frontend = options.frontend;
    let agent_id = options.instance.agent_id.clone();
    let mut writer_task = tokio::spawn(output_loop(frontend, output_rx, stdout, agent_id));
    let mut diagnostic_task = tokio::spawn(diagnostic_loop(diagnostic_rx, stderr));
    let emitters = Emitters {
        frontend,
        output: output_tx,
        diagnostics: diagnostic_tx,
    };

    let client = InstanceClient::spawn(
        PathBuf::from(&options.placement.core_socket),
        options.instance.instance_id.clone(),
        options.instance.revision,
        options.instance.author_id.clone(),
        config_digest(&options.instance.author_id),
    );
    let session_id =
        match establish_session(&client, &options.session, options.connect_timeout).await {
            Ok(session_id) => session_id,
            Err(error) => {
                let code = match &options.session {
                    SessionArg::New(_) => "instance_unavailable",
                    SessionArg::Existing(_) => "session_unavailable",
                };
                let _ = emitters.error(None, code).await;
                drop(emitters);
                join_output(&mut writer_task, "stdout").await?;
                join_output(&mut diagnostic_task, "stderr").await?;
                return Err(error);
            }
        };

    emitters
        .event(Event::Ready {
            agent_id: options.instance.agent_id.clone(),
            session_id: session_id.clone(),
            state: "connected",
        })
        .await?;

    let connection = Arc::new(ConnectionState::new(
        options.instance.agent_id.clone(),
        session_id.clone(),
    ));
    let (activity_tx, activity_rx) = watch::channel(0_usize);
    let (ingress_tx, ingress_rx) = mpsc::channel(QUEUE_CAPACITY);
    let (sender_done_tx, mut sender_done_rx) = oneshot::channel();

    let mut input_task = tokio::spawn(input_loop(
        options.frontend,
        stdin,
        ingress_tx.clone(),
        emitters.clone(),
        connection.clone(),
    ));
    let mut sender_task = tokio::spawn(sender_loop(
        client.clone(),
        session_id.clone(),
        ingress_rx,
        emitters.clone(),
        connection.clone(),
        sender_done_tx,
    ));
    let mut live_task = tokio::spawn(live_loop(
        client.clone(),
        session_id.clone(),
        emitters.clone(),
        connection.clone(),
        activity_tx,
    ));
    let mut monitor_task = tokio::spawn(connection_loop(
        client,
        session_id,
        emitters.clone(),
        connection,
    ));

    enum Stop {
        Requested(anyhow::Result<()>),
        Interrupt,
        Terminate,
        Output(anyhow::Result<()>),
        Diagnostic(anyhow::Result<()>),
    }
    let stop = tokio::select! {
        result = &mut sender_done_rx => Stop::Requested(result.unwrap_or_else(|_| Err(anyhow::anyhow!("sender stopped")))),
        control = controls.recv() => match control {
            Some(Control::Interrupt) => Stop::Interrupt,
            Some(Control::Terminate) => Stop::Terminate,
            None => Stop::Requested(Err(anyhow::anyhow!("signal owner stopped"))),
        },
        result = &mut writer_task => Stop::Output(flatten_join(result, "stdout")),
        result = &mut diagnostic_task => Stop::Diagnostic(flatten_join(result, "stderr")),
    };

    match stop {
        Stop::Interrupt => {
            abort_all(&[&input_task, &sender_task, &live_task, &monitor_task]);
            let _ = (&mut input_task).await;
            let _ = (&mut sender_task).await;
            let _ = (&mut live_task).await;
            let _ = (&mut monitor_task).await;
            drop(ingress_tx);
            drop(emitters);
            join_output(&mut writer_task, "stdout").await?;
            join_output(&mut diagnostic_task, "stderr").await?;
            Ok(Exit::Interrupted)
        }
        Stop::Output(result) | Stop::Diagnostic(result) => {
            abort_all(&[&input_task, &sender_task, &live_task, &monitor_task]);
            result?;
            Err(anyhow::anyhow!("output closed"))
        }
        Stop::Requested(sender_result) => {
            sender_result?;
            finish_graceful(
                activity_rx,
                emitters,
                &mut input_task,
                &mut sender_task,
                &mut live_task,
                &mut monitor_task,
                &mut writer_task,
                &mut diagnostic_task,
            )
            .await
        }
        Stop::Terminate => {
            let drain = async {
                input_task.abort();
                let _ = (&mut input_task).await;
                ingress_tx
                    .send(Input::Shutdown)
                    .await
                    .map_err(|_| anyhow::anyhow!("sender stopped"))?;
                sender_done_rx
                    .await
                    .map_err(|_| anyhow::anyhow!("sender stopped"))??;
                let _ = (&mut sender_task).await;
                wait_inactive(activity_rx).await;
                emitters
                    .event(Event::Closed {
                        reason: "requested",
                    })
                    .await?;
                live_task.abort();
                monitor_task.abort();
                let _ = (&mut live_task).await;
                let _ = (&mut monitor_task).await;
                drop(ingress_tx);
                drop(emitters);
                join_output(&mut writer_task, "stdout").await?;
                join_output(&mut diagnostic_task, "stderr").await?;
                Ok(Exit::Normal)
            };
            match tokio::time::timeout(options.terminate_drain, drain).await {
                Ok(result) => result,
                Err(_) => {
                    abort_all(&[&input_task, &sender_task, &live_task, &monitor_task]);
                    writer_task.abort();
                    diagnostic_task.abort();
                    Err(anyhow::anyhow!("termination drain timed out"))
                }
            }
        }
    }
}

fn abort_all(tasks: &[&tokio::task::JoinHandle<()>]) {
    for task in tasks {
        task.abort();
    }
}

fn flatten_join(
    result: Result<anyhow::Result<()>, tokio::task::JoinError>,
    name: &str,
) -> anyhow::Result<()> {
    result.with_context(|| format!("{name} task join"))?
}

async fn join_output(
    task: &mut tokio::task::JoinHandle<anyhow::Result<()>>,
    name: &str,
) -> anyhow::Result<()> {
    (&mut *task)
        .await
        .with_context(|| format!("{name} task join"))?
}

async fn establish_session(
    client: &Arc<InstanceClient>,
    selector: &SessionArg,
    timeout: Duration,
) -> anyhow::Result<String> {
    tokio::time::timeout(timeout, establish_session_inner(client, selector))
        .await
        .map_err(|_| anyhow::anyhow!("session acknowledgement timed out"))?
}

async fn establish_session_inner(
    client: &Arc<InstanceClient>,
    selector: &SessionArg,
) -> anyhow::Result<String> {
    let session_id = match selector {
        SessionArg::New(name) => {
            let binding_id = uuid::Uuid::new_v4().to_string();
            let address = format!("extgate-{binding_id}");
            loop {
                match client.create_binding(&binding_id, &address, name).await {
                    Ok(()) => break,
                    Err(CreateBindingError::NotReady | CreateBindingError::Disconnected) => {
                        tokio::time::sleep(POLL_INTERVAL).await;
                    }
                    Err(CreateBindingError::Rejected { code }) => {
                        anyhow::bail!("binding rejected: {code}");
                    }
                }
            }
            address
        }
        SessionArg::Existing(address) => address.clone(),
    };
    loop {
        if client.binding_for_address(&session_id).await.is_some() {
            return Ok(session_id);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn input_loop<R: AsyncRead + Unpin>(
    frontend: Frontend,
    stdin: R,
    ingress: mpsc::Sender<Input>,
    emitters: Emitters,
    connection: Arc<ConnectionState>,
) {
    match frontend {
        Frontend::Jsonl => jsonl_input(stdin, ingress, emitters).await,
        Frontend::Repl => repl_input(stdin, ingress, emitters, connection).await,
    }
}

async fn jsonl_input<R: AsyncRead + Unpin>(
    mut reader: R,
    ingress: mpsc::Sender<Input>,
    emitters: Emitters,
) {
    loop {
        match jsonl::read_record(&mut reader).await {
            ReadRecord::Eof => {
                let _ = ingress.send(Input::Shutdown).await;
                break;
            }
            ReadRecord::Record(Ok(input)) => {
                let shutdown = matches!(input, Input::Shutdown);
                if ingress.send(input).await.is_err() || shutdown {
                    break;
                }
            }
            ReadRecord::Record(Err(error)) => {
                if emitters.error(error.request_id, error.code).await.is_err() {
                    break;
                }
            }
        }
    }
}

async fn repl_input<R: AsyncRead + Unpin>(
    reader: R,
    ingress: mpsc::Sender<Input>,
    emitters: Emitters,
    connection: Arc<ConnectionState>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match repl::parse_line(&line) {
            repl::Line::Ignore => {}
            repl::Line::Input(input) => {
                let shutdown = matches!(input, Input::Shutdown);
                if ingress.send(input).await.is_err() || shutdown {
                    return;
                }
            }
            repl::Line::Help => {
                let _ = emitters
                    .output
                    .send(Output::ReplLine(repl::help().into()))
                    .await;
            }
            repl::Line::Status => {
                let state = if connection.is_ready() {
                    "connected"
                } else {
                    "disconnected"
                };
                let _ = emitters
                    .output
                    .send(Output::ReplLine(format!(
                        "{} / {}: {state}",
                        connection.agent_id, connection.session_id
                    )))
                    .await;
            }
            repl::Line::Error(error) => {
                let _ = emitters.diagnostic(&error).await;
            }
        }
    }
    let _ = ingress.send(Input::Shutdown).await;
}

async fn sender_loop(
    client: Arc<InstanceClient>,
    address: String,
    mut ingress: mpsc::Receiver<Input>,
    emitters: Emitters,
    connection: Arc<ConnectionState>,
    done: oneshot::Sender<anyhow::Result<()>>,
) {
    let result = async {
        while let Some(input) = ingress.recv().await {
            match input {
                Input::Shutdown => break,
                Input::Message { id, text } => {
                    let origin = format!("cli:{id}");
                    let context = owner_context();
                    let Some(binding_id) = client.binding_for_address(&address).await else {
                        connection.disconnected_request_error(id, &emitters).await?;
                        continue;
                    };
                    if !said_fits(&binding_id, &origin, &client.author_id, &context, &text) {
                        emitters.error(Some(id), "too_large").await?;
                        continue;
                    }
                    let outcome = client
                        .post_said_with_self_context(&address, &origin, &context, &text, &[])
                        .await;
                    let outcome = match outcome {
                        Err(PostRefuse::NotReady) if !client.connection_live().await => {
                            Ok(SaidOutcome::Disconnected)
                        }
                        other => other,
                    };
                    if matches!(outcome, Ok(SaidOutcome::Disconnected)) {
                        connection.disconnected_request_error(id, &emitters).await?;
                        continue;
                    }
                    emitters.event(map_said(id, origin, outcome)).await?;
                }
            }
        }
        Ok(())
    }
    .await;
    let _ = done.send(result);
}

fn said_fits(
    binding_id: &str,
    origin: &str,
    author_id: &str,
    context: &SaidContext,
    text: &str,
) -> bool {
    let maximum_request_id = format!("said:{}", u64::MAX);
    let frame = said_frame_with_context(
        &maximum_request_id,
        binding_id,
        origin,
        author_id,
        None,
        Some(context),
        text,
        &[],
    );
    serde_json::to_vec(&frame)
        .map(|bytes| bytes.len().saturating_add(1) <= MAX_FRAME)
        .unwrap_or(false)
}

fn owner_context() -> SaidContext {
    SaidContext {
        caller: SaidCaller::Owner,
        start_turn: true,
        system_context: None,
        reply_target: None,
        live_inbound_scope: LiveInboundScope::All,
    }
}

fn map_said(id: String, origin: String, outcome: Result<SaidOutcome, PostRefuse>) -> Event {
    match outcome {
        Ok(SaidOutcome::Accepted { seq }) => Event::Accepted { id, origin, seq },
        Ok(SaidOutcome::NotAdmitted) => Event::NotAdmitted { id },
        Ok(SaidOutcome::Disconnected) => error_event(Some(id), "disconnect"),
        Ok(SaidOutcome::WireErr { code, .. }) => error_event(Some(id), stable_wire_code(&code)),
        Err(PostRefuse::NotReady) => error_event(Some(id), "instance_not_ready"),
        Err(PostRefuse::Busy) => error_event(Some(id), "conversation_busy"),
    }
}

fn stable_wire_code(code: &str) -> &str {
    match code {
        "bad_request"
        | "too_large"
        | "instance_unavailable"
        | "session_unavailable"
        | "instance_not_ready"
        | "not_admitted"
        | "conversation_busy"
        | "disconnect"
        | "gate_error"
        | "output_closed" => code,
        _ => "gate_error",
    }
}

async fn live_loop(
    client: Arc<InstanceClient>,
    address: String,
    emitters: Emitters,
    connection: Arc<ConnectionState>,
    activity: watch::Sender<usize>,
) {
    let mut started = HashSet::new();
    loop {
        if !connection.is_ready() {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }
        let Some(live) = client.next_live(&address).await else {
            continue;
        };
        if matches!(&live, LiveEvent::Error { code, .. } if code == "disconnect") {
            started.clear();
            let _ = activity.send(0);
            let _ = connection.set(false, &emitters).await;
            continue;
        }
        let activity_change = match &live {
            LiveEvent::Activity {
                activity_id, state, ..
            } if state == "started" => Some((true, activity_id.clone())),
            LiveEvent::Activity {
                activity_id, state, ..
            } if state == "ended" => Some((false, activity_id.clone())),
            LiveEvent::TurnFailed { .. } => Some((false, String::new())),
            _ => None,
        };
        if emitters.event(map_live(live)).await.is_err() {
            break;
        }
        if let Some((is_start, activity_id)) = activity_change {
            if is_start {
                started.insert(activity_id);
            } else if activity_id.is_empty() {
                started.clear();
            } else {
                started.remove(&activity_id);
            }
            let _ = activity.send(started.len());
        }
    }
}

fn map_live(event: LiveEvent) -> Event {
    match event {
        LiveEvent::Message {
            delivery_id,
            text,
            reply_origin,
        } => Event::Message {
            delivery_id,
            text,
            reply_origin,
        },
        LiveEvent::Activity {
            activity_id,
            state,
            origin,
        } => Event::Activity {
            activity_id,
            state,
            origin,
        },
        LiveEvent::Completed { target } => Event::Completed { target },
        LiveEvent::CompletedNoReply { reply_origin } => Event::CompletedNoReply { reply_origin },
        LiveEvent::TurnFailed { reply_origin } => Event::TurnFailed { reply_origin },
        LiveEvent::Error { code, .. } => error_event(None, stable_wire_code(&code)),
    }
}

async fn connection_loop(
    client: Arc<InstanceClient>,
    address: String,
    emitters: Emitters,
    connection: Arc<ConnectionState>,
) {
    loop {
        let current = client.binding_for_address(&address).await.is_some();
        if connection.set(current, &emitters).await.is_err() {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_graceful(
    activity: watch::Receiver<usize>,
    emitters: Emitters,
    input_task: &mut tokio::task::JoinHandle<()>,
    sender_task: &mut tokio::task::JoinHandle<()>,
    live_task: &mut tokio::task::JoinHandle<()>,
    monitor_task: &mut tokio::task::JoinHandle<()>,
    writer_task: &mut tokio::task::JoinHandle<anyhow::Result<()>>,
    diagnostic_task: &mut tokio::task::JoinHandle<anyhow::Result<()>>,
) -> anyhow::Result<Exit> {
    input_task.abort();
    let _ = (&mut *input_task).await;
    let _ = (&mut *sender_task).await;
    wait_inactive(activity).await;
    emitters
        .event(Event::Closed {
            reason: "requested",
        })
        .await?;
    live_task.abort();
    monitor_task.abort();
    let _ = (&mut *live_task).await;
    let _ = (&mut *monitor_task).await;
    drop(emitters);
    join_output(writer_task, "stdout").await?;
    join_output(diagnostic_task, "stderr").await?;
    Ok(Exit::Normal)
}

async fn wait_inactive(mut activity: watch::Receiver<usize>) {
    loop {
        if *activity.borrow() == 0 {
            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL) => return,
                changed = activity.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        } else if activity.changed().await.is_err() {
            return;
        }
    }
}

async fn output_loop<W: AsyncWrite + Unpin>(
    frontend: Frontend,
    mut queue: mpsc::Receiver<Output>,
    mut writer: W,
    agent_id: String,
) -> anyhow::Result<()> {
    while let Some(output) = queue.recv().await {
        match frontend {
            Frontend::Repl => repl::write_output(&mut writer, &output, &agent_id).await?,
            Frontend::Jsonl => match output {
                Output::Event(event) => {
                    let mut bytes = serde_json::to_vec(&event)?;
                    bytes.push(b'\n');
                    writer.write_all(&bytes).await?;
                    writer.flush().await?;
                }
                Output::ReplLine(_) => anyhow::bail!("REPL output entered JSONL writer"),
            },
        }
    }
    writer.flush().await?;
    Ok(())
}

async fn diagnostic_loop<W: AsyncWrite + Unpin>(
    mut queue: mpsc::Receiver<String>,
    mut writer: W,
) -> anyhow::Result<()> {
    while let Some(line) = queue.recv().await {
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    writer.flush().await?;
    Ok(())
}

fn error_event(request_id: Option<String>, code: &str) -> Event {
    Event::Error {
        request_id,
        code: code.to_string(),
        detail: None,
    }
}

#[cfg(test)]
#[path = "runtime/tests.rs"]
mod tests;
