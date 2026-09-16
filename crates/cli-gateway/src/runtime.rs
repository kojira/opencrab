use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use opencrab_gate_client::client::{
    CreateBindingError, InstanceClient, LiveEvent, PostRefuse, SaidOutcome,
};
use opencrab_gate_client::wire::{config_digest, LiveInboundScope, SaidCaller, SaidContext};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

use crate::args::SessionArg;
use crate::config::{InstancePlacement, Placement};
use crate::jsonl::{self, Event, Input, Output, ReadRecord};
use crate::repl;

const QUEUE_CAPACITY: usize = 32;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const TERMINATE_DRAIN: Duration = Duration::from_secs(10);

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
}

pub async fn run<R, W>(
    options: RunOptions,
    stdin: R,
    stdout: W,
    mut controls: mpsc::Receiver<Control>,
) -> anyhow::Result<Exit>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (output_tx, output_rx) = mpsc::channel(QUEUE_CAPACITY);
    let (writer_done_tx, mut writer_done_rx) = oneshot::channel();
    let frontend = options.frontend;
    let agent_id = options.instance.agent_id.clone();
    let writer_task = tokio::spawn(async move {
        let result = output_loop(frontend, output_rx, stdout, &agent_id).await;
        let _ = writer_done_tx.send(result);
    });

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
                let _ = emit_error(&output_tx, None, code).await;
                drop(output_tx);
                let _ = writer_task.await;
                return Err(error);
            }
        };

    send_output(
        &output_tx,
        Event::Ready {
            agent_id: options.instance.agent_id.clone(),
            session_id: session_id.clone(),
            state: "connected",
        },
    )
    .await?;

    let ready = Arc::new(AtomicBool::new(true));
    let (activity_tx, activity_rx) = watch::channel(0_usize);
    let (ingress_tx, ingress_rx) = mpsc::channel(QUEUE_CAPACITY);
    let (sender_done_tx, mut sender_done_rx) = oneshot::channel();

    let input_task = tokio::spawn(input_loop(
        options.frontend,
        stdin,
        ingress_tx.clone(),
        output_tx.clone(),
        ready.clone(),
        options.instance.agent_id.clone(),
        session_id.clone(),
    ));
    let sender_task = tokio::spawn(sender_loop(
        client.clone(),
        session_id.clone(),
        ingress_rx,
        output_tx.clone(),
        sender_done_tx,
    ));
    let live_task = tokio::spawn(live_loop(
        client.clone(),
        session_id.clone(),
        output_tx.clone(),
        ready.clone(),
        activity_tx,
    ));
    let monitor_task = tokio::spawn(connection_loop(
        client,
        session_id,
        options.instance.agent_id,
        output_tx.clone(),
        ready,
    ));

    enum Stop {
        Requested(anyhow::Result<()>),
        Interrupt,
        Terminate,
        Output(anyhow::Result<()>),
    }
    let stop = tokio::select! {
        result = &mut sender_done_rx => Stop::Requested(result.unwrap_or_else(|_| Err(anyhow::anyhow!("sender stopped")))),
        control = controls.recv() => match control {
            Some(Control::Interrupt) => Stop::Interrupt,
            Some(Control::Terminate) => Stop::Terminate,
            None => Stop::Requested(Err(anyhow::anyhow!("signal owner stopped"))),
        },
        result = &mut writer_done_rx => Stop::Output(result.unwrap_or_else(|_| Err(anyhow::anyhow!("output task stopped")))),
    };

    match stop {
        Stop::Interrupt => {
            input_task.abort();
            sender_task.abort();
            live_task.abort();
            monitor_task.abort();
            drop(ingress_tx);
            drop(output_tx);
            let _ = writer_task.await;
            Ok(Exit::Interrupted)
        }
        Stop::Output(result) => {
            input_task.abort();
            sender_task.abort();
            live_task.abort();
            monitor_task.abort();
            result.context("stdout")?;
            Err(anyhow::anyhow!("output closed"))
        }
        Stop::Requested(sender_result) => {
            sender_result?;
            finish_graceful(
                activity_rx,
                output_tx,
                input_task,
                live_task,
                monitor_task,
                writer_task,
            )
            .await
        }
        Stop::Terminate => {
            input_task.abort();
            let drain = async {
                ingress_tx
                    .send(Input::Shutdown)
                    .await
                    .map_err(|_| anyhow::anyhow!("sender stopped"))?;
                sender_done_rx
                    .await
                    .map_err(|_| anyhow::anyhow!("sender stopped"))??;
                wait_inactive(activity_rx).await;
                send_output(
                    &output_tx,
                    Event::Closed {
                        reason: "requested",
                    },
                )
                .await
            };
            let result = tokio::time::timeout(TERMINATE_DRAIN, drain)
                .await
                .map_err(|_| anyhow::anyhow!("termination drain timed out"))?;
            result?;
            live_task.abort();
            monitor_task.abort();
            drop(output_tx);
            writer_task.await.context("output join")?;
            Ok(Exit::Normal)
        }
    }
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
    output: mpsc::Sender<Output>,
    ready: Arc<AtomicBool>,
    agent_id: String,
    session_id: String,
) {
    match frontend {
        Frontend::Jsonl => jsonl_input(stdin, ingress, output).await,
        Frontend::Repl => repl_input(stdin, ingress, output, ready, agent_id, session_id).await,
    }
}

async fn jsonl_input<R: AsyncRead + Unpin>(
    mut reader: R,
    ingress: mpsc::Sender<Input>,
    output: mpsc::Sender<Output>,
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
                if emit_error(&output, error.request_id, error.code)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

async fn repl_input<R: AsyncRead + Unpin>(
    reader: R,
    ingress: mpsc::Sender<Input>,
    output: mpsc::Sender<Output>,
    ready: Arc<AtomicBool>,
    agent_id: String,
    session_id: String,
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
                let _ = output.send(Output::ReplLine(repl::help().into())).await;
            }
            repl::Line::Status => {
                let state = if ready.load(Ordering::SeqCst) {
                    "connected"
                } else {
                    "disconnected"
                };
                let _ = output
                    .send(Output::ReplLine(format!(
                        "{agent_id} / {session_id}: {state}"
                    )))
                    .await;
            }
            repl::Line::Error(error) => {
                let _ = output.send(Output::ReplLine(error)).await;
            }
        }
    }
    let _ = ingress.send(Input::Shutdown).await;
}

async fn sender_loop(
    client: Arc<InstanceClient>,
    address: String,
    mut ingress: mpsc::Receiver<Input>,
    output: mpsc::Sender<Output>,
    done: oneshot::Sender<anyhow::Result<()>>,
) {
    let result = async {
        while let Some(input) = ingress.recv().await {
            match input {
                Input::Shutdown => break,
                Input::Message { id, text } => {
                    let origin = format!("cli:{id}");
                    let context = owner_context();
                    let outcome = client
                        .post_said_with_self_context(&address, &origin, &context, &text, &[])
                        .await;
                    let outcome = match outcome {
                        Err(PostRefuse::NotReady) if !client.connection_live().await => {
                            Ok(SaidOutcome::Disconnected)
                        }
                        other => other,
                    };
                    let event = map_said(id, origin, outcome);
                    send_output(&output, event).await?;
                }
            }
        }
        Ok(())
    }
    .await;
    let _ = done.send(result);
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
        Ok(SaidOutcome::WireErr { code, .. }) => {
            let stable = stable_wire_code(&code);
            error_event(Some(id), stable)
        }
        Err(PostRefuse::NotReady) => error_event(Some(id), "instance_not_ready"),
        Err(PostRefuse::Busy) => error_event(Some(id), "conversation_busy"),
    }
}

fn stable_wire_code(code: &str) -> &str {
    match code {
        "bad_request"
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
    output: mpsc::Sender<Output>,
    ready: Arc<AtomicBool>,
    activity: watch::Sender<usize>,
) {
    let mut started = HashSet::new();
    loop {
        if !ready.load(Ordering::SeqCst) {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }
        let Some(live) = client.next_live(&address).await else {
            continue;
        };
        if matches!(&live, LiveEvent::Error { code, .. } if code == "disconnect") {
            started.clear();
            let _ = activity.send(0);
            tokio::time::sleep(POLL_INTERVAL).await;
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
        if send_output(&output, map_live(live)).await.is_err() {
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
    agent_id: String,
    output: mpsc::Sender<Output>,
    ready: Arc<AtomicBool>,
) {
    let mut previous = true;
    loop {
        let current = client.binding_for_address(&address).await.is_some();
        if current != previous {
            ready.store(current, Ordering::SeqCst);
            let state = if current { "connected" } else { "disconnected" };
            if send_output(&output, Event::Connection { state })
                .await
                .is_err()
            {
                break;
            }
            if current
                && send_output(
                    &output,
                    Event::Ready {
                        agent_id: agent_id.clone(),
                        session_id: address.clone(),
                        state: "connected",
                    },
                )
                .await
                .is_err()
            {
                break;
            }
            previous = current;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn finish_graceful(
    activity: watch::Receiver<usize>,
    output: mpsc::Sender<Output>,
    input_task: tokio::task::JoinHandle<()>,
    live_task: tokio::task::JoinHandle<()>,
    monitor_task: tokio::task::JoinHandle<()>,
    writer_task: tokio::task::JoinHandle<()>,
) -> anyhow::Result<Exit> {
    input_task.abort();
    wait_inactive(activity).await;
    send_output(
        &output,
        Event::Closed {
            reason: "requested",
        },
    )
    .await?;
    live_task.abort();
    monitor_task.abort();
    drop(output);
    writer_task.await.context("output join")?;
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
    agent_id: &str,
) -> anyhow::Result<()> {
    while let Some(output) = queue.recv().await {
        match frontend {
            Frontend::Repl => repl::write_output(&mut writer, &output, agent_id).await?,
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

async fn send_output(output: &mpsc::Sender<Output>, event: Event) -> anyhow::Result<()> {
    output
        .send(Output::Event(event))
        .await
        .map_err(|_| anyhow::anyhow!("output_closed"))
}

async fn emit_error(
    output: &mpsc::Sender<Output>,
    request_id: Option<String>,
    code: &str,
) -> anyhow::Result<()> {
    send_output(output, error_event(request_id, code)).await
}

fn error_event(request_id: Option<String>, code: &str) -> Event {
    Event::Error {
        request_id,
        code: code.to_string(),
        detail: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_context_is_exact() {
        assert_eq!(owner_context().caller, SaidCaller::Owner);
        assert!(owner_context().start_turn);
        assert_eq!(owner_context().system_context, None);
        assert_eq!(owner_context().reply_target, None);
        assert_eq!(owner_context().live_inbound_scope, LiveInboundScope::All);
    }

    #[test]
    fn maps_acceptance_and_redacts_wire_details() {
        let id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_string();
        let origin = format!("cli:{id}");
        assert_eq!(
            map_said(
                id.clone(),
                origin.clone(),
                Ok(SaidOutcome::Accepted { seq: 4 })
            ),
            Event::Accepted {
                id: id.clone(),
                origin,
                seq: 4
            }
        );
        assert_eq!(
            map_said(
                id.clone(),
                format!("cli:{id}"),
                Ok(SaidOutcome::WireErr {
                    code: "private_sql_error".into(),
                    detail: Some("secret".into())
                })
            ),
            Event::Error {
                request_id: Some(id),
                code: "gate_error".into(),
                detail: None
            }
        );
    }

    #[test]
    fn maps_all_live_contract_variants() {
        assert!(matches!(
            map_live(LiveEvent::Message {
                delivery_id: "d".into(),
                text: "reply".into(),
                reply_origin: None,
            }),
            Event::Message { .. }
        ));
        assert!(matches!(
            map_live(LiveEvent::Activity {
                activity_id: "a".into(),
                state: "read".into(),
                origin: Some("cli:x".into()),
            }),
            Event::Activity { .. }
        ));
        assert!(matches!(
            map_live(LiveEvent::CompletedNoReply { reply_origin: None }),
            Event::CompletedNoReply { reply_origin: None }
        ));
        assert!(matches!(
            map_live(LiveEvent::TurnFailed {
                reply_origin: "cli:x".into()
            }),
            Event::TurnFailed { .. }
        ));
        assert!(matches!(
            map_live(LiveEvent::Completed { target: "d".into() }),
            Event::Completed { .. }
        ));
    }

    #[tokio::test]
    async fn bounded_output_writer_reports_broken_pipe() {
        assert_eq!(QUEUE_CAPACITY, 32);
        let (writer, reader) = tokio::io::duplex(64);
        drop(reader);
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        tx.send(Output::Event(Event::Closed {
            reason: "requested",
        }))
        .await
        .unwrap();
        drop(tx);
        assert!(output_loop(Frontend::Jsonl, rx, writer, "agent-a")
            .await
            .is_err());
    }
}
