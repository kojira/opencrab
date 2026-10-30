//! UDS listen と接続状態機械。PRE_HELLO / RUNNING / CLOSED のみ。

mod activity;
mod hello;
mod interrupt;
mod provisional;
mod response;

pub use activity::{emit_activity, emit_ended_activity, emit_turn_failed};

use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use opencrab_actions::{AgentRuntime, ModelAdministration};
use rusqlite::{params, Connection, TransactionBehavior};
use tokio::io::BufReader;
use tokio::net::UnixListener;

use crate::close::{close_all_lives, close_live};
use crate::commands::{CommandContext, CommandError};
use crate::error::{ErrorCode, GateError};
use crate::ids::now_nanos;
use crate::inbound::{process_said, resolve};
use crate::protocol::{
    command_err_frame, command_ok_frame, err_frame, ok_open_frame, ok_said_frame, read_frame,
    write_json, Command, FrameError, InboundMsg, Open, SaidCaller,
};
use crate::registry::ExtgateState;

use hello::handle_hello;
use response::handle_response;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const SOCKET_MODE: u32 = 0o660;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnState {
    PreHello,
    Running,
}

/// 空は listen しない。相対・既存の非 socket は失敗。既存 socket は unlink。
pub fn validate_listen_socket(raw: &str) -> Result<Option<PathBuf>, anyhow::Error> {
    if raw.is_empty() {
        return Ok(None);
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        anyhow::bail!("gate.listen_socket must be an absolute path");
    }
    if path.exists() {
        let meta = std::fs::metadata(path)?;
        if !meta.file_type().is_socket() {
            anyhow::bail!("gate.listen_socket exists and is not a socket");
        }
        std::fs::remove_file(path)?;
    }
    Ok(Some(path.to_path_buf()))
}

/// HTTP/UDS より前に exact 1 回。
pub fn recover_stale_deliveries(conn: &mut Connection, now: i64) -> Result<(), GateError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| GateError::store())?;
    tx.execute(
        "UPDATE deliveries
         SET state = 'indeterminate',
             error = 'stale sending recovered after restart',
             updated_at = ?1
         WHERE state = 'sending'",
        params![now],
    )
    .map_err(|_| GateError::store())?;
    tx.commit().map_err(|_| GateError::store())?;
    Ok(())
}

pub async fn serve_uds<R: AgentRuntime + ModelAdministration>(
    state: Arc<ExtgateState>,
    runtime: R,
    path: PathBuf,
) -> Result<(), anyhow::Error> {
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(SOCKET_MODE))?;
    loop {
        if state.is_halted() {
            close_all_lives(&state, ErrorCode::Disconnect).await;
            anyhow::bail!("extgate listener halted");
        }
        let accepted = tokio::select! {
            _ = state.wait_until_halted() => {
                close_all_lives(&state, ErrorCode::Disconnect).await;
                anyhow::bail!("extgate listener halted");
            }
            accepted = listener.accept() => accepted,
        };
        if state.is_halted() {
            close_all_lives(&state, ErrorCode::Disconnect).await;
            anyhow::bail!("extgate listener halted");
        }
        let (stream, _) = accepted?;
        let conn_state = Arc::clone(&state);
        let halt = Arc::clone(&state);
        let runtime = runtime.clone();
        let task = tokio::spawn(async move {
            handle_connection(conn_state, runtime, stream).await;
        });
        tokio::spawn(async move {
            if task.await.is_err() {
                tracing::error!("extgate connection task panicked");
                close_all_lives(&halt, ErrorCode::Disconnect).await;
                halt.halt();
            }
        });
    }
}

async fn handle_connection<R: AgentRuntime + ModelAdministration>(
    state: Arc<ExtgateState>,
    runtime: R,
    stream: tokio::net::UnixStream,
) {
    let (read, write) = stream.into_split();
    let writer = Arc::new(tokio::sync::Mutex::new(write));
    let mut reader = BufReader::new(read);
    let identity = state.alloc_identity();
    let mut phase = ConnState::PreHello;
    let mut instance_id: Option<String> = None;

    let hello_result = tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut reader)).await;
    match hello_result {
        Err(_) => {
            close_live(
                &state,
                None,
                Some(identity),
                ErrorCode::ProtocolOrder,
                None,
                Some(&writer),
            )
            .await;
            return;
        }
        Ok(Err(FrameError::TooLarge)) => {
            close_live(
                &state,
                None,
                Some(identity),
                ErrorCode::TooLarge,
                None,
                Some(&writer),
            )
            .await;
            return;
        }
        Ok(Err(FrameError::Eof)) | Ok(Err(FrameError::Io)) => {
            return;
        }
        Ok(Ok(bytes)) => {
            let mut ctx = ConnCtx {
                state: &state,
                runtime: &runtime,
                writer: &writer,
                phase: &mut phase,
                instance_id: &mut instance_id,
                identity,
            };
            if dispatch_frame(&mut ctx, &bytes).await.is_err() {
                return;
            }
        }
    }

    loop {
        if state.is_halted() {
            close_live(
                &state,
                instance_id.as_deref(),
                Some(identity),
                ErrorCode::Disconnect,
                None,
                None,
            )
            .await;
            return;
        }
        match read_frame(&mut reader).await {
            Err(FrameError::TooLarge) => {
                close_live(
                    &state,
                    instance_id.as_deref(),
                    Some(identity),
                    ErrorCode::TooLarge,
                    None,
                    Some(&writer),
                )
                .await;
                return;
            }
            Err(FrameError::Eof) | Err(FrameError::Io) => {
                close_live(
                    &state,
                    instance_id.as_deref(),
                    Some(identity),
                    ErrorCode::Disconnect,
                    None,
                    None,
                )
                .await;
                return;
            }
            Ok(bytes) => {
                let mut ctx = ConnCtx {
                    state: &state,
                    runtime: &runtime,
                    writer: &writer,
                    phase: &mut phase,
                    instance_id: &mut instance_id,
                    identity,
                };
                if dispatch_frame(&mut ctx, &bytes).await.is_err() {
                    return;
                }
            }
        }
    }
}

struct ConnCtx<'a, R> {
    state: &'a Arc<ExtgateState>,
    runtime: &'a R,
    writer: &'a Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    phase: &'a mut ConnState,
    instance_id: &'a mut Option<String>,
    identity: u64,
}

async fn dispatch_frame<R: AgentRuntime + ModelAdministration>(
    ctx: &mut ConnCtx<'_, R>,
    bytes: &[u8],
) -> Result<(), ()> {
    let state = ctx.state;
    let runtime = ctx.runtime;
    let writer = ctx.writer;
    let identity = ctx.identity;
    let phase = &mut *ctx.phase;
    let instance_id = &mut *ctx.instance_id;
    let parsed = match crate::protocol::parse_frame_bytes(bytes) {
        Ok(m) => m,
        Err(e) => {
            close_live(
                state,
                instance_id.as_deref(),
                Some(identity),
                e.code,
                None,
                Some(writer),
            )
            .await;
            return Err(());
        }
    };
    match (*phase, parsed) {
        (ConnState::PreHello, InboundMsg::Hello(hello)) => {
            match handle_hello(state, writer, identity, hello).await {
                Ok(id) => {
                    *phase = ConnState::Running;
                    *instance_id = Some(id);
                    Ok(())
                }
                Err(()) => Err(()),
            }
        }
        (ConnState::PreHello, InboundMsg::Open(request)) => {
            close_live(
                state,
                None,
                Some(identity),
                ErrorCode::ProtocolOrder,
                Some(&request.id),
                Some(writer),
            )
            .await;
            Err(())
        }
        (ConnState::PreHello, InboundMsg::Command(command)) => {
            close_live(
                state,
                None,
                Some(identity),
                ErrorCode::ProtocolOrder,
                Some(&command.id),
                Some(writer),
            )
            .await;
            Err(())
        }
        (ConnState::PreHello, InboundMsg::Provisional(provisional)) => {
            close_live(
                state,
                None,
                Some(identity),
                ErrorCode::ProtocolOrder,
                Some(&provisional.id),
                Some(writer),
            )
            .await;
            Err(())
        }
        (ConnState::PreHello, InboundMsg::ProvisionalCancel(cancel)) => {
            close_live(
                state,
                None,
                Some(identity),
                ErrorCode::ProtocolOrder,
                Some(&cancel.id),
                Some(writer),
            )
            .await;
            Err(())
        }
        (ConnState::PreHello, InboundMsg::Interrupt(interrupt)) => {
            close_live(
                state,
                None,
                Some(identity),
                ErrorCode::ProtocolOrder,
                Some(&interrupt.id),
                Some(writer),
            )
            .await;
            Err(())
        }
        (ConnState::PreHello, InboundMsg::Said(said)) => {
            close_live(
                state,
                None,
                Some(identity),
                ErrorCode::ProtocolOrder,
                Some(&said.id),
                Some(writer),
            )
            .await;
            Err(())
        }
        (ConnState::PreHello, InboundMsg::Response(resp)) => {
            close_live(
                state,
                None,
                Some(identity),
                ErrorCode::ProtocolOrder,
                Some(&resp.id),
                Some(writer),
            )
            .await;
            Err(())
        }
        (ConnState::PreHello, InboundMsg::Reverse { id, .. } | InboundMsg::Unknown { id, .. }) => {
            close_live(
                state,
                None,
                Some(identity),
                ErrorCode::ProtocolOrder,
                id.as_deref(),
                Some(writer),
            )
            .await;
            Err(())
        }
        (ConnState::PreHello, InboundMsg::Invalid { id, code, m }) => {
            let reason = if m == "hello" {
                code
            } else {
                ErrorCode::ProtocolOrder
            };
            close_live(
                state,
                None,
                Some(identity),
                reason,
                id.as_deref(),
                Some(writer),
            )
            .await;
            Err(())
        }
        (ConnState::Running, InboundMsg::Hello(hello)) => {
            close_live(
                state,
                instance_id.as_deref(),
                Some(identity),
                ErrorCode::ProtocolOrder,
                Some(&hello.id),
                Some(writer),
            )
            .await;
            Err(())
        }
        (ConnState::Running, InboundMsg::Open(request)) => {
            let inst = instance_id.as_deref().expect("running has instance");
            handle_open(state, writer, inst, identity, &request).await
        }
        (ConnState::Running, InboundMsg::Command(command)) => {
            let inst = instance_id.as_deref().expect("running has instance");
            handle_command(state, runtime, writer, inst, identity, command).await
        }
        (ConnState::Running, InboundMsg::Provisional(provisional)) => {
            let inst = instance_id.as_deref().expect("running has instance");
            provisional::handle_provisional(state, runtime, writer, inst, identity, &provisional)
                .await
        }
        (ConnState::Running, InboundMsg::ProvisionalCancel(cancel)) => {
            let inst = instance_id.as_deref().expect("running has instance");
            provisional::handle_provisional_cancel(state, writer, inst, identity, &cancel).await
        }
        (ConnState::Running, InboundMsg::Interrupt(interrupt)) => {
            let inst = instance_id.as_deref().expect("running has instance");
            interrupt::handle_interrupt(state, writer, inst, identity, &interrupt).await
        }
        (ConnState::Running, InboundMsg::Said(said)) => {
            let inst = instance_id.as_deref().expect("running has instance");
            match process_said(state, inst, &said, runtime) {
                Ok(out) => {
                    if write_json(writer, &ok_said_frame(&said.id, out.seq, &out.session_id))
                        .await
                        .is_err()
                    {
                        close_live(
                            state,
                            Some(inst),
                            Some(identity),
                            ErrorCode::Disconnect,
                            None,
                            None,
                        )
                        .await;
                        return Err(());
                    }
                    Ok(())
                }
                Err(e) if e.code == ErrorCode::StoreError => {
                    close_live(
                        state,
                        Some(inst),
                        Some(identity),
                        ErrorCode::StoreError,
                        Some(&said.id),
                        Some(writer),
                    )
                    .await;
                    Err(())
                }
                Err(e) => {
                    if write_json(writer, &err_frame(&said.id, e.code, e.detail.as_deref()))
                        .await
                        .is_err()
                    {
                        close_live(
                            state,
                            Some(inst),
                            Some(identity),
                            ErrorCode::Disconnect,
                            None,
                            None,
                        )
                        .await;
                        return Err(());
                    }
                    Ok(())
                }
            }
        }
        (ConnState::Running, InboundMsg::Response(resp)) => {
            handle_response(
                state,
                writer,
                instance_id.as_deref().unwrap(),
                identity,
                resp,
            )
            .await
        }
        (ConnState::Running, InboundMsg::Invalid { id, code, m }) => {
            let inst = instance_id.as_deref().unwrap();
            if m == "hello" {
                close_live(
                    state,
                    Some(inst),
                    Some(identity),
                    ErrorCode::ProtocolOrder,
                    id.as_deref(),
                    Some(writer),
                )
                .await;
                return Err(());
            }
            if code == ErrorCode::ResponseInvalid {
                close_live(
                    state,
                    Some(inst),
                    Some(identity),
                    ErrorCode::ResponseInvalid,
                    id.as_deref(),
                    Some(writer),
                )
                .await;
                return Err(());
            }
            match id {
                Some(id) => {
                    if write_json(writer, &err_frame(&id, code, None))
                        .await
                        .is_err()
                    {
                        close_live(
                            state,
                            Some(inst),
                            Some(identity),
                            ErrorCode::Disconnect,
                            None,
                            None,
                        )
                        .await;
                        return Err(());
                    }
                    Ok(())
                }
                None => Ok(()),
            }
        }
        (ConnState::Running, InboundMsg::Reverse { id, .. } | InboundMsg::Unknown { id, .. }) => {
            let inst = instance_id.as_deref().unwrap();
            match id {
                Some(id) => {
                    if write_json(writer, &err_frame(&id, ErrorCode::UnknownMessage, None))
                        .await
                        .is_err()
                    {
                        close_live(
                            state,
                            Some(inst),
                            Some(identity),
                            ErrorCode::Disconnect,
                            None,
                            None,
                        )
                        .await;
                        return Err(());
                    }
                    Ok(())
                }
                None => Ok(()),
            }
        }
    }
}

async fn handle_command<R: ModelAdministration>(
    state: &ExtgateState,
    runtime: &R,
    writer: &tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>,
    instance_id: &str,
    identity: u64,
    command: Command,
) -> Result<(), ()> {
    // Model administration intentionally has a stricter boundary than ordinary said admission.
    let outcome = if !matches!(command.caller, SaidCaller::Owner) {
        Err(CommandError::forbidden())
    } else {
        // D-1099: the command target is the key's core session (resolved, never a client value).
        let target = match state.db.lock() {
            Ok(mut conn) => resolve(&mut conn, instance_id, &command.session_key, None)
                .map(|row| (row.agent_id, row.session_id)),
            Err(_) => Err(crate::error::GateError::store()),
        };
        let live = state
            .lock_registry()
            .ok()
            .and_then(|registry| registry.get(instance_id).map(|live| live.identity == identity))
            .unwrap_or(false);
        match target {
            Ok(_) if !live => Err(CommandError::binding_not_found()),
            Ok((agent_id, session_id)) => {
                state
                    .commands
                    .dispatch(
                        &command.name,
                        CommandContext {
                            agent_id: &agent_id,
                            session_id: &session_id,
                            caller: &command.caller,
                            models: runtime,
                        },
                        &command.args,
                    )
                    .await
            }
            Err(error) if error.code == ErrorCode::InstanceUnknown => {
                Err(CommandError::binding_not_found())
            }
            Err(error) => {
                tracing::error!(code = error.code.as_str(), "model command session lookup failed");
                Err(CommandError::internal())
            }
        }
    };

    let response = match outcome {
        Ok(result) => command_ok_frame(&command.id, &result),
        Err(error) => command_err_frame(&command.id, error.code, &error.message),
    };
    write_json(writer, &response).await.map_err(|_| ())
}

/// D-1099: `open` — the core session of a key, created when new. Idempotent.
async fn handle_open(
    state: &Arc<ExtgateState>,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    instance_id: &str,
    identity: u64,
    request: &Open,
) -> Result<(), ()> {
    let resolved = match state.db.lock() {
        Ok(mut conn) => resolve(
            &mut conn,
            instance_id,
            &request.session_key,
            request.title.as_deref(),
        ),
        Err(_) => Err(crate::error::GateError::store()),
    };
    let frame = match resolved {
        Ok(row) => ok_open_frame(&request.id, &row.session_id),
        Err(e) if e.code == ErrorCode::StoreError => {
            close_live(
                state,
                Some(instance_id),
                Some(identity),
                ErrorCode::StoreError,
                Some(&request.id),
                Some(writer),
            )
            .await;
            return Err(());
        }
        Err(e) => err_frame(&request.id, e.code, e.detail.as_deref()),
    };
    if write_json(writer, &frame).await.is_err() {
        close_live(
            state,
            Some(instance_id),
            Some(identity),
            ErrorCode::Disconnect,
            None,
            None,
        )
        .await;
        return Err(());
    }
    Ok(())
}

pub fn recover_now(conn: &mut Connection) -> Result<(), GateError> {
    recover_stale_deliveries(conn, now_nanos())
}
