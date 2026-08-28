//! 1 instance = 1 UDS connection。hello / bind ack / said / say / activity だけ。
//! 切断後は指数 backoff で再接続し、hello 再送で open binding を replay する。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};

use super::wire::{
    err_frame, hello_frame, ok_frame, parse_frame_bytes, read_frame, said_frame, say_text,
    write_json, Activity, Attachment, Bind, CoreMsg, FrameError, Say, WireResponse,
};

const LIVE_QUEUE_CAP: usize = 32;
/// said 応答の上限。V3 の hello 10s と同じクラス（said ack は LLM を待たない）。
const SAID_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_MIN: Duration = Duration::from_millis(200);
const RECONNECT_MAX: Duration = Duration::from_secs(8);

struct WriteOut {
    tx: mpsc::UnboundedSender<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEvent {
    Message {
        text: String,
    },
    Activity {
        activity_id: String,
        state: String,
    },
    CompletedNoReply,
    Error {
        code: String,
        detail: Option<String>,
    },
}

#[derive(Debug)]
pub enum SaidOutcome {
    Accepted {
        seq: i64,
    },
    NotAdmitted,
    WireErr {
        code: String,
        detail: Option<String>,
    },
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostRefuse {
    NotReady,
    Busy,
}

/// core からの `say` をどう扱うか。Web は live queue へ受理、Nostr 第1段は投稿能力が無いので拒否する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SayPolicy {
    AcceptToLiveQueue,
    RejectExternal,
}

enum PendingKind {
    Hello,
    Said,
}

struct PendingSaid {
    kind: PendingKind,
    reply: oneshot::Sender<SaidOutcome>,
}

struct PendingTurn {
    saw_say: bool,
    origin: String,
}

struct LiveQueue {
    events: std::collections::VecDeque<LiveEvent>,
    waiters: Vec<oneshot::Sender<LiveEvent>>,
}

impl LiveQueue {
    fn new() -> Self {
        Self {
            events: std::collections::VecDeque::new(),
            waiters: Vec::new(),
        }
    }

    fn try_push(&mut self, ev: LiveEvent) -> bool {
        if let Some(idx) = self.waiters.iter().position(|w| !w.is_closed()) {
            let waiter = self.waiters.remove(idx);
            if waiter.send(ev.clone()).is_ok() {
                return true;
            }
        }
        if self.events.len() >= LIVE_QUEUE_CAP {
            return false;
        }
        self.events.push_back(ev);
        true
    }

    fn subscribe(&mut self) -> Option<LiveEvent> {
        self.events.pop_front()
    }
}

struct Inner {
    acknowledged: HashMap<String, String>,
    remembered: HashMap<String, String>,
    pending_said: HashMap<String, PendingSaid>,
    pending_turn: HashMap<String, PendingTurn>,
    live: HashMap<String, LiveQueue>,
    closed: bool,
    generation: u64,
}

pub struct InstanceClient {
    pub instance_id: String,
    pub author_id: String,
    say_policy: SayPolicy,
    inner: Mutex<Inner>,
    write: Mutex<WriteOut>,
    closed_notify: Notify,
    req_seq: AtomicU64,
}

impl InstanceClient {
    fn blank(instance_id: String, author_id: String, say_policy: SayPolicy) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        Self {
            instance_id,
            author_id,
            say_policy,
            inner: Mutex::new(Inner {
                acknowledged: HashMap::new(),
                remembered: HashMap::new(),
                pending_said: HashMap::new(),
                pending_turn: HashMap::new(),
                live: HashMap::new(),
                closed: true,
                generation: 0,
            }),
            write: Mutex::new(WriteOut { tx }),
            closed_notify: Notify::new(),
            req_seq: AtomicU64::new(1),
        }
    }

    /// 1 回だけ connect。切断後の再接続はしない（conformance 用）。
    pub async fn connect(
        socket: &std::path::Path,
        instance_id: String,
        revision: u64,
        author_id: String,
        config_digest: String,
    ) -> Result<Arc<Self>, FrameError> {
        let client = Arc::new(Self::blank(
            instance_id,
            author_id,
            SayPolicy::AcceptToLiveQueue,
        ));
        attach(&client, socket, revision, &config_digest).await?;
        Ok(client)
    }

    /// HTTP を先に生かし、UDS は指数 backoff でつなぎ続ける。
    pub fn spawn(
        socket: PathBuf,
        instance_id: String,
        revision: u64,
        author_id: String,
        config_digest: String,
    ) -> Arc<Self> {
        Self::spawn_with_say_policy(
            socket,
            instance_id,
            revision,
            author_id,
            config_digest,
            SayPolicy::AcceptToLiveQueue,
        )
    }

    pub fn spawn_with_say_policy(
        socket: PathBuf,
        instance_id: String,
        revision: u64,
        author_id: String,
        config_digest: String,
        say_policy: SayPolicy,
    ) -> Arc<Self> {
        let client = Arc::new(Self::blank(instance_id, author_id, say_policy));
        tokio::spawn(reconnect_loop(
            client.clone(),
            socket,
            revision,
            config_digest,
        ));
        client
    }

    pub async fn connection_live(&self) -> bool {
        !self.inner.lock().await.closed
    }

    pub async fn remembered_binding(&self, address: &str) -> Option<String> {
        self.inner.lock().await.remembered.get(address).cloned()
    }

    fn next_id(&self) -> String {
        let n = self.req_seq.fetch_add(1, Ordering::Relaxed);
        format!("said:{n}")
    }

    pub async fn binding_for_address(&self, address: &str) -> Option<String> {
        let inner = self.inner.lock().await;
        if inner.closed {
            return None;
        }
        inner.acknowledged.get(address).cloned()
    }

    pub async fn post_said(
        &self,
        address: &str,
        origin: &str,
        text: &str,
        attachments: &[Attachment],
    ) -> Result<SaidOutcome, PostRefuse> {
        self.post_said_with_author(address, origin, &self.author_id, text, attachments)
            .await
    }

    pub async fn post_said_with_author(
        &self,
        address: &str,
        origin: &str,
        author_id: &str,
        text: &str,
        attachments: &[Attachment],
    ) -> Result<SaidOutcome, PostRefuse> {
        self.post_said_inner(address, origin, author_id, text, attachments, true)
            .await
    }

    /// Bundle member 用。ack までだけ binding を占有する。
    ///
    /// Accepted のあと turn が始まらない（coordinator が全 receipt 待ち）ときに
    /// 次の origin を送れる。ターン中の占有は activity started が立てる。
    pub async fn post_said_receipt(
        &self,
        address: &str,
        origin: &str,
        author_id: &str,
        text: &str,
        attachments: &[Attachment],
    ) -> Result<SaidOutcome, PostRefuse> {
        self.post_said_inner(address, origin, author_id, text, attachments, false)
            .await
    }

    async fn post_said_inner(
        &self,
        address: &str,
        origin: &str,
        author_id: &str,
        text: &str,
        attachments: &[Attachment],
        occupy_until_turn_ends: bool,
    ) -> Result<SaidOutcome, PostRefuse> {
        let binding_id = {
            let mut inner = self.inner.lock().await;
            if inner.closed {
                return Err(PostRefuse::NotReady);
            }
            let Some(binding_id) = inner.acknowledged.get(address).cloned() else {
                return Err(PostRefuse::NotReady);
            };
            if let Some(turn) = inner.pending_turn.get(&binding_id) {
                if turn.origin != origin {
                    return Err(PostRefuse::Busy);
                }
            } else {
                inner.pending_turn.insert(
                    binding_id.clone(),
                    PendingTurn {
                        saw_say: false,
                        origin: origin.to_string(),
                    },
                );
            }
            binding_id
        };
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut inner = self.inner.lock().await;
            inner.pending_said.insert(
                id.clone(),
                PendingSaid {
                    kind: PendingKind::Said,
                    reply: tx,
                },
            );
        }
        let frame = said_frame(&id, &binding_id, origin, author_id, text, attachments);
        tracing::info!(
            instance_id = %self.instance_id,
            binding_id = %binding_id,
            origin,
            "said"
        );
        if !send_frame(self, frame).await {
            let mut inner = self.inner.lock().await;
            inner.pending_turn.remove(&binding_id);
            inner.pending_said.remove(&id);
            return Ok(SaidOutcome::Disconnected);
        }
        match tokio::time::timeout(SAID_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => {
                if !occupy_until_turn_ends || !matches!(outcome, SaidOutcome::Accepted { .. }) {
                    let mut inner = self.inner.lock().await;
                    inner.pending_turn.remove(&binding_id);
                }
                tracing::info!(
                    instance_id = %self.instance_id,
                    binding_id = %binding_id,
                    "said ack"
                );
                Ok(outcome)
            }
            Ok(Err(_)) | Err(_) => {
                let generation = self.inner.lock().await.generation;
                close_all(self, "disconnect", generation).await;
                Ok(SaidOutcome::Disconnected)
            }
        }
    }

    pub async fn next_live(&self, address: &str) -> Option<LiveEvent> {
        let rx = {
            let mut inner = self.inner.lock().await;
            if inner.closed {
                if let Some(q) = inner.live.get_mut(address) {
                    if let Some(ev) = q.subscribe() {
                        return Some(ev);
                    }
                }
                return Some(LiveEvent::Error {
                    code: "disconnect".into(),
                    detail: None,
                });
            }
            if let Some(q) = inner.live.get_mut(address) {
                if let Some(ev) = q.subscribe() {
                    return Some(ev);
                }
            }
            let (tx, rx) = oneshot::channel();
            let q = inner
                .live
                .entry(address.to_string())
                .or_insert_with(LiveQueue::new);
            q.waiters.push(tx);
            rx
        };
        rx.await.ok()
    }
}

async fn send_frame(client: &InstanceClient, value: Value) -> bool {
    client.write.lock().await.tx.send(value).is_ok()
}

async fn attach(
    client: &Arc<InstanceClient>,
    socket: &std::path::Path,
    revision: u64,
    config_digest: &str,
) -> Result<(), FrameError> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|_| FrameError::Io)?;
    let (reader, writer) = stream.into_split();
    let (write_tx, write_rx) = mpsc::unbounded_channel();
    let generation = {
        let mut inner = client.inner.lock().await;
        inner.generation = inner.generation.saturating_add(1);
        inner.closed = false;
        inner.acknowledged.clear();
        inner.pending_said.clear();
        inner.pending_turn.clear();
        inner.live.clear();
        inner.generation
    };
    *client.write.lock().await = WriteOut { tx: write_tx };
    tokio::spawn(write_loop(writer, write_rx));
    let hello_id = format!("hello:{}", client.instance_id);
    if !send_frame(
        client,
        hello_frame(&hello_id, &client.instance_id, revision, config_digest),
    )
    .await
    {
        return Err(FrameError::Io);
    }
    let (hello_tx, hello_rx) = oneshot::channel();
    {
        let mut inner = client.inner.lock().await;
        inner.pending_said.insert(
            hello_id,
            PendingSaid {
                kind: PendingKind::Hello,
                reply: hello_tx,
            },
        );
    }
    tokio::spawn(read_loop(reader, client.clone(), generation));
    tracing::info!(instance_id = %client.instance_id, "hello");
    match hello_rx.await {
        Ok(SaidOutcome::Accepted { .. }) | Ok(SaidOutcome::NotAdmitted) => {
            tracing::info!(instance_id = %client.instance_id, "hello ok");
            Ok(())
        }
        Ok(SaidOutcome::WireErr { .. }) | Ok(SaidOutcome::Disconnected) | Err(_) => {
            tracing::info!(instance_id = %client.instance_id, "hello failed");
            Err(FrameError::Io)
        }
    }
}

async fn reconnect_loop(
    client: Arc<InstanceClient>,
    socket: PathBuf,
    revision: u64,
    config_digest: String,
) {
    let mut backoff = RECONNECT_MIN;
    loop {
        match attach(&client, &socket, revision, &config_digest).await {
            Ok(()) => {
                tracing::info!(instance_id = %client.instance_id, "uds connected");
                backoff = RECONNECT_MIN;
                let notified = client.closed_notify.notified();
                if client.inner.lock().await.closed {
                    tracing::info!(instance_id = %client.instance_id, "uds closed during hello");
                } else {
                    notified.await;
                    tracing::info!(instance_id = %client.instance_id, "uds closed; reconnecting");
                }
            }
            Err(_) => {
                tracing::warn!(
                    instance_id = %client.instance_id,
                    "uds connect/hello failed"
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = backoff.saturating_mul(2).min(RECONNECT_MAX);
    }
}

async fn write_loop(mut writer: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<Value>) {
    while let Some(value) = rx.recv().await {
        if write_json(&mut writer, &value).await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

async fn read_loop(
    mut reader: tokio::net::unix::OwnedReadHalf,
    client: Arc<InstanceClient>,
    generation: u64,
) {
    loop {
        match read_frame(&mut reader).await {
            Ok(bytes) => match parse_frame_bytes(&bytes) {
                Ok(msg) => {
                    if handle_msg(&client, msg, generation).await {
                        break;
                    }
                }
                Err(FrameError::BadRequest) | Err(FrameError::TooLarge) => {
                    close_all(&client, "bad_request", generation).await;
                    break;
                }
                Err(_) => {
                    close_all(&client, "disconnect", generation).await;
                    break;
                }
            },
            Err(FrameError::TooLarge) => {
                close_all(&client, "too_large", generation).await;
                break;
            }
            Err(_) => {
                close_all(&client, "disconnect", generation).await;
                break;
            }
        }
    }
}

async fn handle_msg(client: &InstanceClient, msg: CoreMsg, generation: u64) -> bool {
    match msg {
        CoreMsg::Bind(bind) => {
            handle_bind(client, bind, generation).await;
            false
        }
        CoreMsg::Say(say) => handle_say(client, say, generation).await,
        CoreMsg::Activity(activity) => {
            handle_activity(client, activity).await;
            false
        }
        CoreMsg::Response(resp) => {
            handle_response(client, resp, generation).await;
            false
        }
        CoreMsg::Reverse { id, .. } | CoreMsg::Unknown { id, .. } => {
            if let Some(id) = id {
                let _ = send_frame(client, err_frame(&id, "unknown_message", None)).await;
            }
            false
        }
        CoreMsg::Invalid { id, code, .. } => {
            if let Some(id) = id {
                let _ = send_frame(client, err_frame(&id, code, None)).await;
            }
            if code == "response_invalid" {
                close_all(client, "response_invalid", generation).await;
                return true;
            }
            false
        }
    }
}

async fn handle_bind(client: &InstanceClient, bind: Bind, generation: u64) {
    tracing::info!(
        instance_id = %client.instance_id,
        binding_id = %bind.binding_id,
        address = %bind.address,
        "bind"
    );
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return;
    }
    if let Some(existing) = inner.acknowledged.get(&bind.address) {
        if existing != &bind.binding_id {
            drop(inner);
            close_all(client, "binding_conflict", generation).await;
            return;
        }
    }
    inner
        .remembered
        .insert(bind.address.clone(), bind.binding_id.clone());
    inner
        .acknowledged
        .insert(bind.address.clone(), bind.binding_id);
    inner
        .live
        .entry(bind.address)
        .or_insert_with(LiveQueue::new);
    drop(inner);
    let _ = send_frame(client, ok_frame(&bind.id)).await;
}

async fn handle_say(client: &InstanceClient, say: Say, generation: u64) -> bool {
    tracing::info!(
        instance_id = %client.instance_id,
        binding_id = %say.binding_id,
        "say"
    );
    if client.say_policy == SayPolicy::RejectExternal {
        let _ = send_frame(client, err_frame(&say.id, "external_rejected", None)).await;
        return false;
    }
    let Some(text) = say_text(&say.payload).map(str::to_string) else {
        let _ = send_frame(client, err_frame(&say.id, "external_rejected", None)).await;
        return false;
    };
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return true;
    }
    let address = inner
        .acknowledged
        .iter()
        .find(|(_, bid)| *bid == &say.binding_id)
        .map(|(a, _)| a.clone());
    let Some(address) = address else {
        drop(inner);
        let _ = send_frame(client, err_frame(&say.id, "external_rejected", None)).await;
        return false;
    };
    let q = inner
        .live
        .entry(address.clone())
        .or_insert_with(LiveQueue::new);
    let accepted = q.try_push(LiveEvent::Message { text });
    if !accepted {
        drop(inner);
        let _ = send_frame(client, err_frame(&say.id, "external_rejected", None)).await;
        return false;
    }
    if let Some(turn) = inner.pending_turn.get_mut(&say.binding_id) {
        turn.saw_say = true;
    }
    drop(inner);
    if !send_frame(client, ok_frame(&say.id)).await {
        close_all(client, "disconnect", generation).await;
        return true;
    }
    false
}

async fn handle_activity(client: &InstanceClient, activity: Activity) {
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return;
    }
    let address = inner
        .acknowledged
        .iter()
        .find(|(_, bid)| *bid == &activity.binding_id)
        .map(|(a, _)| a.clone());
    let Some(address) = address else {
        return;
    };
    let q = inner
        .live
        .entry(address.clone())
        .or_insert_with(LiveQueue::new);
    let _ = q.try_push(LiveEvent::Activity {
        activity_id: activity.activity_id,
        state: activity.state.clone(),
    });
    if activity.state == "started" {
        inner
            .pending_turn
            .entry(activity.binding_id.clone())
            .or_insert_with(|| PendingTurn {
                saw_say: false,
                origin: String::new(),
            });
    } else if activity.state == "ended" {
        if let Some(turn) = inner.pending_turn.remove(&activity.binding_id) {
            if !turn.saw_say {
                if let Some(q) = inner.live.get_mut(&address) {
                    let _ = q.try_push(LiveEvent::CompletedNoReply);
                }
            }
        }
    }
}

async fn handle_response(client: &InstanceClient, resp: WireResponse, generation: u64) {
    let mut inner = client.inner.lock().await;
    let Some(pending) = inner.pending_said.remove(&resp.id) else {
        drop(inner);
        close_all(client, "response_invalid", generation).await;
        return;
    };
    let outcome = match pending.kind {
        PendingKind::Hello => {
            if resp.ok && resp.seq.is_none() {
                SaidOutcome::Accepted { seq: 0 }
            } else if !resp.ok {
                SaidOutcome::WireErr {
                    code: resp.code.unwrap_or_else(|| "bad_request".into()),
                    detail: resp.detail,
                }
            } else {
                drop(inner);
                close_all(client, "response_invalid", generation).await;
                return;
            }
        }
        PendingKind::Said => {
            if resp.ok {
                match resp.seq {
                    Some(Some(seq)) => SaidOutcome::Accepted { seq },
                    Some(None) => SaidOutcome::NotAdmitted,
                    None => {
                        drop(inner);
                        close_all(client, "response_invalid", generation).await;
                        return;
                    }
                }
            } else {
                SaidOutcome::WireErr {
                    code: resp.code.unwrap_or_else(|| "bad_request".into()),
                    detail: resp.detail,
                }
            }
        }
    };
    let _ = pending.reply.send(outcome);
}

async fn close_all(client: &InstanceClient, code: &str, generation: u64) {
    let mut inner = client.inner.lock().await;
    if inner.closed || inner.generation != generation {
        return;
    }
    inner.closed = true;
    let drained: Vec<(String, String)> = inner.acknowledged.drain().collect();
    for (address, binding_id) in drained {
        inner.remembered.insert(address, binding_id);
    }
    for (_, pending) in inner.pending_said.drain() {
        let _ = pending.reply.send(SaidOutcome::Disconnected);
    }
    inner.pending_turn.clear();
    let ev = LiveEvent::Error {
        code: code.to_string(),
        detail: None,
    };
    for q in inner.live.values_mut() {
        let _ = q.try_push(ev.clone());
        q.waiters.clear();
    }
    drop(inner);
    tracing::info!(instance_id = %client.instance_id, code, "close");
    client.closed_notify.notify_waiters();
}
