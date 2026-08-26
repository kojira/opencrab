//! 1 instance = 1 UDS connection。hello / bind ack / said / say / activity だけ。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{Mutex, mpsc, oneshot};

use super::wire::{
    Activity, Attachment, Bind, CoreMsg, FrameError, Say, WireResponse, err_frame, hello_frame,
    ok_frame, parse_frame_bytes, read_frame, said_frame, say_text, write_json,
};

const LIVE_QUEUE_CAP: usize = 32;
/// said 応答の上限。V3 の hello 10s と同じクラス（said ack は LLM を待たない）。
const SAID_TIMEOUT: Duration = Duration::from_secs(10);

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
    pending_said: HashMap<String, PendingSaid>,
    pending_turn: HashMap<String, PendingTurn>,
    live: HashMap<String, LiveQueue>,
    closed: bool,
}

pub struct InstanceClient {
    pub instance_id: String,
    pub author_id: String,
    inner: Mutex<Inner>,
    write_tx: mpsc::UnboundedSender<Value>,
    req_seq: AtomicU64,
}

impl InstanceClient {
    pub async fn connect(
        socket: &std::path::Path,
        instance_id: String,
        revision: u64,
        author_id: String,
        config_digest: String,
    ) -> Result<Arc<Self>, FrameError> {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|_| FrameError::Io)?;
        let (reader, writer) = stream.into_split();
        let (write_tx, write_rx) = mpsc::unbounded_channel();
        let client = Arc::new(Self {
            instance_id: instance_id.clone(),
            author_id,
            inner: Mutex::new(Inner {
                acknowledged: HashMap::new(),
                pending_said: HashMap::new(),
                pending_turn: HashMap::new(),
                live: HashMap::new(),
                closed: false,
            }),
            write_tx,
            req_seq: AtomicU64::new(1),
        });
        tokio::spawn(write_loop(writer, write_rx));
        let hello_id = format!("hello:{}", client.instance_id);
        client
            .write_tx
            .send(hello_frame(
                &hello_id,
                &instance_id,
                revision,
                &config_digest,
            ))
            .map_err(|_| FrameError::Io)?;
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
        tokio::spawn(read_loop(reader, client.clone()));
        match hello_rx.await {
            Ok(SaidOutcome::Accepted { .. }) | Ok(SaidOutcome::NotAdmitted) => {}
            Ok(SaidOutcome::WireErr { .. }) | Ok(SaidOutcome::Disconnected) | Err(_) => {
                return Err(FrameError::Io);
            }
        }
        Ok(client)
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
        let frame = said_frame(&id, &binding_id, origin, &self.author_id, text, attachments);
        if self.write_tx.send(frame).is_err() {
            let mut inner = self.inner.lock().await;
            inner.pending_turn.remove(&binding_id);
            inner.pending_said.remove(&id);
            return Ok(SaidOutcome::Disconnected);
        }
        match tokio::time::timeout(SAID_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => {
                if !matches!(outcome, SaidOutcome::Accepted { .. }) {
                    let mut inner = self.inner.lock().await;
                    inner.pending_turn.remove(&binding_id);
                }
                Ok(outcome)
            }
            Ok(Err(_)) | Err(_) => {
                close_all(self, "disconnect").await;
                Ok(SaidOutcome::Disconnected)
            }
        }
    }

    pub async fn next_live(&self, address: &str) -> Option<LiveEvent> {
        let rx = {
            let mut inner = self.inner.lock().await;
            if inner.closed {
                return None;
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

async fn write_loop(mut writer: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<Value>) {
    while let Some(value) = rx.recv().await {
        if write_json(&mut writer, &value).await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

async fn read_loop(mut reader: tokio::net::unix::OwnedReadHalf, client: Arc<InstanceClient>) {
    loop {
        match read_frame(&mut reader).await {
            Ok(bytes) => match parse_frame_bytes(&bytes) {
                Ok(msg) => {
                    if handle_msg(&client, msg).await {
                        break;
                    }
                }
                Err(FrameError::BadRequest) | Err(FrameError::TooLarge) => {
                    close_all(&client, "bad_request").await;
                    break;
                }
                Err(_) => {
                    close_all(&client, "disconnect").await;
                    break;
                }
            },
            Err(FrameError::TooLarge) => {
                close_all(&client, "too_large").await;
                break;
            }
            Err(_) => {
                close_all(&client, "disconnect").await;
                break;
            }
        }
    }
}

async fn handle_msg(client: &InstanceClient, msg: CoreMsg) -> bool {
    match msg {
        CoreMsg::Bind(bind) => {
            handle_bind(client, bind).await;
            false
        }
        CoreMsg::Say(say) => handle_say(client, say).await,
        CoreMsg::Activity(activity) => {
            handle_activity(client, activity).await;
            false
        }
        CoreMsg::Response(resp) => {
            handle_response(client, resp).await;
            false
        }
        CoreMsg::Reverse { id, .. } | CoreMsg::Unknown { id, .. } => {
            if let Some(id) = id {
                let _ = client
                    .write_tx
                    .send(err_frame(&id, "unknown_message", None));
            }
            false
        }
        CoreMsg::Invalid { id, code, .. } => {
            if let Some(id) = id {
                let _ = client.write_tx.send(err_frame(&id, code, None));
            }
            if code == "response_invalid" {
                close_all(client, "response_invalid").await;
                return true;
            }
            false
        }
    }
}

async fn handle_bind(client: &InstanceClient, bind: Bind) {
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return;
    }
    if let Some(existing) = inner.acknowledged.get(&bind.address) {
        if existing != &bind.binding_id {
            drop(inner);
            close_all(client, "binding_conflict").await;
            return;
        }
    }
    inner
        .acknowledged
        .insert(bind.address.clone(), bind.binding_id);
    inner
        .live
        .entry(bind.address)
        .or_insert_with(LiveQueue::new);
    let _ = client.write_tx.send(ok_frame(&bind.id));
}

async fn handle_say(client: &InstanceClient, say: Say) -> bool {
    let Some(text) = say_text(&say.payload).map(str::to_string) else {
        let _ = client
            .write_tx
            .send(err_frame(&say.id, "external_rejected", None));
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
        let _ = client
            .write_tx
            .send(err_frame(&say.id, "external_rejected", None));
        return false;
    };
    let q = inner
        .live
        .entry(address.clone())
        .or_insert_with(LiveQueue::new);
    let accepted = q.try_push(LiveEvent::Message { text });
    if !accepted {
        drop(inner);
        let _ = client
            .write_tx
            .send(err_frame(&say.id, "external_rejected", None));
        return false;
    }
    if let Some(turn) = inner.pending_turn.get_mut(&say.binding_id) {
        turn.saw_say = true;
    }
    drop(inner);
    if client.write_tx.send(ok_frame(&say.id)).is_err() {
        close_all(client, "disconnect").await;
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
    if activity.state == "ended" {
        if let Some(turn) = inner.pending_turn.remove(&activity.binding_id) {
            if !turn.saw_say {
                if let Some(q) = inner.live.get_mut(&address) {
                    let _ = q.try_push(LiveEvent::CompletedNoReply);
                }
            }
        }
    }
}

async fn handle_response(client: &InstanceClient, resp: WireResponse) {
    let mut inner = client.inner.lock().await;
    let Some(pending) = inner.pending_said.remove(&resp.id) else {
        drop(inner);
        close_all(client, "response_invalid").await;
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
                close_all(client, "response_invalid").await;
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
                        close_all(client, "response_invalid").await;
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

async fn close_all(client: &InstanceClient, code: &str) {
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return;
    }
    inner.closed = true;
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
}
