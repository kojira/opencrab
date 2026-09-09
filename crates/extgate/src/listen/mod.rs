//! UDS listen と接続状態機械。PRE_HELLO / RUNNING / CLOSED のみ。

mod activity;
mod bind;
mod hello;
mod response;

pub use activity::{emit_activity, emit_turn_failed};
pub use bind::{enqueue_bind, wait_bind_ack, web_binding_state, EnqueueBindOutcome};

use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use opencrab_actions::AgentRuntime;
use rusqlite::{params, Connection, TransactionBehavior};
use tokio::io::BufReader;
use tokio::net::UnixListener;

use crate::close::{close_all_lives, close_live};
use crate::error::{ErrorCode, GateError};
use crate::ids::now_nanos;
use crate::inbound::process_said;
use crate::protocol::{err_frame, ok_said_frame, read_frame, write_json, FrameError, InboundMsg};
use crate::registry::ExtgateState;
use crate::ResolveCallerFn;

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

pub async fn serve_uds<R: AgentRuntime>(
    state: Arc<ExtgateState>,
    runtime: R,
    resolve_caller: ResolveCallerFn,
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
            handle_connection(conn_state, runtime, resolve_caller, stream).await;
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

async fn handle_connection<R: AgentRuntime>(
    state: Arc<ExtgateState>,
    runtime: R,
    resolve_caller: ResolveCallerFn,
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
                resolve_caller,
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
                    resolve_caller,
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
    resolve_caller: ResolveCallerFn,
    writer: &'a Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    phase: &'a mut ConnState,
    instance_id: &'a mut Option<String>,
    identity: u64,
}

async fn dispatch_frame<R: AgentRuntime>(ctx: &mut ConnCtx<'_, R>, bytes: &[u8]) -> Result<(), ()> {
    let state = ctx.state;
    let runtime = ctx.runtime;
    let resolve_caller = ctx.resolve_caller;
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
        (ConnState::Running, InboundMsg::Said(said)) => {
            let inst = instance_id.as_deref().expect("running has instance");
            match process_said(state, inst, &said, resolve_caller, runtime) {
                Ok(out) => {
                    if write_json(writer, &ok_said_frame(&said.id, out.seq))
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

pub fn recover_now(conn: &mut Connection) -> Result<(), GateError> {
    recover_stale_deliveries(conn, now_nanos())
}
