//! UDS listen と接続状態機械。PRE_HELLO / RUNNING / CLOSED のみ。

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
use crate::delivery::{mark_delivered, mark_failed, mark_indeterminate};
use crate::error::{ErrorCode, GateError};
use crate::ids::now_nanos;
use crate::inbound::process_said;
use crate::protocol::{
    activity_frame, bind_frame, err_frame, ok_frame, ok_said_frame, read_frame, write_json,
    FrameError, InboundMsg, WireResponse,
};
use crate::registry::{ExtgateState, LiveEntry, Pending};
use crate::ResolveCallerFn;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const BIND_TIMEOUT: Duration = Duration::from_secs(60);
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
            handle_response(state, writer, instance_id.as_deref().unwrap(), identity, resp).await
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

async fn handle_hello(
    state: &Arc<ExtgateState>,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    identity: u64,
    hello: crate::protocol::Hello,
) -> Result<String, ()> {
    if hello.protocol != 2 {
        close_live(
            state,
            None,
            Some(identity),
            ErrorCode::ProtocolUnsupported,
            Some(&hello.id),
            Some(writer),
        )
        .await;
        return Err(());
    }
    let registered = {
        let mut reg = match state.lock_registry() {
            Ok(g) => g,
            Err(_) => return Err(()),
        };
        if reg.is_live(&hello.instance_id) {
            Err(ErrorCode::InstanceActive)
        } else {
            match inspect_instance(&state.db, &hello.instance_id) {
                Err(code) => Err(code),
                Ok(inspected) if !inspected.enabled => Err(ErrorCode::InstanceDisabled),
                Ok(inspected) if inspected.revision != hello.revision => {
                    Err(ErrorCode::RevisionMismatch)
                }
                Ok(inspected) if inspected.config_digest != hello.config_digest => {
                    Err(ErrorCode::ConfigDigestMismatch)
                }
                Ok(_) => match open_bindings(&state.db, &hello.instance_id) {
                    Err(_) => Err(ErrorCode::StoreError),
                    Ok(bindings) => {
                        let mut pending = std::collections::HashMap::new();
                        let started = std::time::Instant::now();
                        for (binding_id, _) in &bindings {
                            pending.insert(
                                crate::ids::bind_request_id(binding_id),
                                Pending::Bind {
                                    binding_id: binding_id.clone(),
                                    started,
                                },
                            );
                        }
                        reg.insert(
                            hello.instance_id.clone(),
                            LiveEntry {
                                identity,
                                revision: hello.revision,
                                writer: Arc::clone(writer),
                                acknowledged: std::collections::HashSet::new(),
                                pending,
                            },
                        );
                        Ok(bindings)
                    }
                },
            }
        }
    };
    let bindings = match registered {
        Ok(b) => b,
        Err(code) => {
            close_live(
                state,
                None,
                Some(identity),
                code,
                Some(&hello.id),
                Some(writer),
            )
            .await;
            return Err(());
        }
    };

    if write_json(writer, &ok_frame(&hello.id)).await.is_err() {
        close_live(
            state,
            Some(&hello.instance_id),
            Some(identity),
            ErrorCode::Disconnect,
            None,
            None,
        )
        .await;
        return Err(());
    }
    for (binding_id, address) in &bindings {
        if write_json(writer, &bind_frame(binding_id, address))
            .await
            .is_err()
        {
            close_live(
                state,
                Some(&hello.instance_id),
                Some(identity),
                ErrorCode::BindFailed,
                None,
                None,
            )
            .await;
            return Err(());
        }
        spawn_bind_timeout(Arc::clone(state), hello.instance_id.clone(), binding_id.clone(), identity);
    }
    Ok(hello.instance_id)
}

fn spawn_bind_timeout(
    state: Arc<ExtgateState>,
    instance_id: String,
    binding_id: String,
    identity: u64,
) {
    tokio::spawn(async move {
        tokio::time::sleep(BIND_TIMEOUT).await;
        let still = {
            let Ok(reg) = state.lock_registry() else {
                return;
            };
            match reg.get(&instance_id) {
                Some(e) if e.identity == identity => e
                    .pending
                    .get(&crate::ids::bind_request_id(&binding_id))
                    .is_some_and(Pending::is_bind),
                _ => false,
            }
        };
        if still {
            close_live(
                &state,
                Some(&instance_id),
                Some(identity),
                ErrorCode::BindFailed,
                None,
                None,
            )
            .await;
        }
    });
}

struct InstanceSnap {
    enabled: bool,
    revision: u64,
    config_digest: String,
}

fn inspect_instance(db: &opencrab_db::Db, instance_id: &str) -> Result<InstanceSnap, ErrorCode> {
    let conn = db.lock().map_err(|_| ErrorCode::StoreError)?;
    match conn.query_row(
        "SELECT enabled, revision, config_digest, deleted_at
         FROM gate_instances WHERE instance_id = ?1",
        params![instance_id],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        },
    ) {
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(ErrorCode::InstanceUnknown),
        Err(_) => Err(ErrorCode::StoreError),
        Ok((_, _, _, Some(_))) => Err(ErrorCode::InstanceUnknown),
        Ok((enabled, revision, digest, None)) => Ok(InstanceSnap {
            enabled: enabled == 1,
            revision: u64::try_from(revision).map_err(|_| ErrorCode::StoreError)?,
            config_digest: digest,
        }),
    }
}

fn open_bindings(db: &opencrab_db::Db, instance_id: &str) -> Result<Vec<(String, String)>, GateError> {
    let conn = db.lock().map_err(|_| GateError::store())?;
    let mut stmt = conn
        .prepare(
            "SELECT binding_id, address FROM gate_bindings
             WHERE instance_id = ?1 AND closed_at IS NULL
             ORDER BY binding_id ASC",
        )
        .map_err(|_| GateError::store())?;
    let rows = stmt
        .query_map(params![instance_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|_| GateError::store())?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|_| GateError::store())?);
    }
    Ok(out)
}

async fn handle_response(
    state: &Arc<ExtgateState>,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    instance_id: &str,
    identity: u64,
    resp: WireResponse,
) -> Result<(), ()> {
    let pending = {
        let mut reg = match state.lock_registry() {
            Ok(g) => g,
            Err(_) => return Err(()),
        };
        let Some(live) = reg.get_mut(instance_id) else {
            return Err(());
        };
        live.pending.remove(&resp.id)
    };
    let Some(pending) = pending else {
        close_live(
            state,
            Some(instance_id),
            Some(identity),
            ErrorCode::ResponseInvalid,
            Some(&resp.id),
            Some(writer),
        )
        .await;
        return Err(());
    };
    match pending {
        Pending::Bind { binding_id, .. } => {
            if resp.ok {
                if resp.seq.is_some() {
                    close_live(
                        state,
                        Some(instance_id),
                        Some(identity),
                        ErrorCode::ResponseInvalid,
                        Some(&resp.id),
                        Some(writer),
                    )
                    .await;
                    return Err(());
                }
                let mut reg = match state.lock_registry() {
                    Ok(g) => g,
                    Err(_) => return Err(()),
                };
                if let Some(live) = reg.get_mut(instance_id) {
                    live.acknowledged.insert(binding_id);
                }
                Ok(())
            } else if resp.code == Some(ErrorCode::BindFailed) {
                close_live(
                    state,
                    Some(instance_id),
                    Some(identity),
                    ErrorCode::BindFailed,
                    None,
                    None,
                )
                .await;
                Err(())
            } else {
                close_live(
                    state,
                    Some(instance_id),
                    Some(identity),
                    ErrorCode::ResponseInvalid,
                    Some(&resp.id),
                    Some(writer),
                )
                .await;
                Err(())
            }
        }
        Pending::Say { delivery_id } => {
            if resp.ok {
                if resp.seq.is_some() {
                    if let Err(e) = mark_indeterminate(state, &[delivery_id]) {
                        tracing::error!(code = e.code.as_str(), "indeterminate after invalid say ok");
                        state.halt();
                    }
                    close_live(
                        state,
                        Some(instance_id),
                        Some(identity),
                        ErrorCode::ResponseInvalid,
                        Some(&resp.id),
                        Some(writer),
                    )
                    .await;
                    return Err(());
                }
                if let Err(e) = mark_delivered(state, &delivery_id) {
                    tracing::error!(code = e.code.as_str(), "delivered write failed");
                    if let Err(ind) = mark_indeterminate(state, std::slice::from_ref(&delivery_id)) {
                        tracing::error!(code = ind.code.as_str(), "indeterminate after delivered write failed");
                    }
                    close_live(
                        state,
                        Some(instance_id),
                        Some(identity),
                        ErrorCode::StoreError,
                        None,
                        None,
                    )
                    .await;
                    state.halt();
                    return Err(());
                }
                Ok(())
            } else if resp.code == Some(ErrorCode::ExternalRejected) {
                if let Err(e) = mark_failed(state, &delivery_id) {
                    tracing::error!(code = e.code.as_str(), "failed write failed");
                    if let Err(ind) = mark_indeterminate(state, std::slice::from_ref(&delivery_id)) {
                        tracing::error!(code = ind.code.as_str(), "indeterminate after failed write failed");
                    }
                    close_live(
                        state,
                        Some(instance_id),
                        Some(identity),
                        ErrorCode::StoreError,
                        None,
                        None,
                    )
                    .await;
                    state.halt();
                    return Err(());
                }
                Ok(())
            } else {
                if let Err(e) = mark_indeterminate(state, &[delivery_id]) {
                    tracing::error!(code = e.code.as_str(), "indeterminate after invalid say err");
                    state.halt();
                }
                close_live(
                    state,
                    Some(instance_id),
                    Some(identity),
                    ErrorCode::ResponseInvalid,
                    Some(&resp.id),
                    Some(writer),
                )
                .await;
                Err(())
            }
        }
    }
}

/// 当該 binding が acknowledged になるまで待つ。live 消失は即 false。
pub async fn wait_bind_ack(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let Ok(reg) = state.lock_registry() else {
                return false;
            };
            match reg.get(instance_id) {
                Some(live) if live.acknowledged.contains(binding_id) => return true,
                Some(_) => {}
                None => return false,
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// open binding と registry から Web 投影の state を導出する。
pub fn web_binding_state(reg: &crate::registry::Registry, instance_id: &str, binding_id: &str) -> &'static str {
    match reg.get(instance_id) {
        Some(live) if live.acknowledged.contains(binding_id) => "ready",
        Some(live)
            if live
                .pending
                .values()
                .any(|p| p.binding_id() == Some(binding_id)) =>
        {
            "provisioning"
        }
        Some(_) => "provisioning",
        None => "unavailable",
    }
}

/// 新規 Binding PUT 後の bind exact 1。
pub async fn enqueue_bind(state: &Arc<ExtgateState>, instance_id: &str, binding_id: &str, address: &str) {
    let (writer, identity) = {
        let mut reg = match state.lock_registry() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(live) = reg.get_mut(instance_id) else {
            return;
        };
        let req_id = crate::ids::bind_request_id(binding_id);
        if live.pending.contains_key(&req_id) || live.acknowledged.contains(binding_id) {
            return;
        }
        live.pending.insert(
            req_id,
            Pending::Bind {
                binding_id: binding_id.to_string(),
                started: std::time::Instant::now(),
            },
        );
        (live.writer.clone(), live.identity)
    };
    crate::race::park("after_pending").await;
    if write_json(&writer, &bind_frame(binding_id, address))
        .await
        .is_err()
    {
        close_live(
            state,
            Some(instance_id),
            Some(identity),
            ErrorCode::BindFailed,
            None,
            None,
        )
        .await;
        return;
    }
    spawn_bind_timeout(
        Arc::clone(state),
        instance_id.to_string(),
        binding_id.to_string(),
        identity,
    );
}

pub async fn emit_activity(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    activity_id: &str,
    activity_state: &str,
) {
    let writer = {
        let Ok(reg) = state.lock_registry() else {
            return;
        };
        let Some(live) = reg.get(instance_id) else {
            return;
        };
        if !live.acknowledged.contains(binding_id) {
            return;
        }
        live.writer.clone()
    };
    let _ = write_json(
        &writer,
        &activity_frame(binding_id, activity_id, activity_state),
    )
    .await;
}

pub fn recover_now(conn: &mut Connection) -> Result<(), GateError> {
    recover_stale_deliveries(conn, now_nanos())
}
