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
    err_frame, hello_frame_with_operations, invoke_ok_frame, ok_frame, parse_frame_bytes,
    read_frame, said_frame, say_reply_target, say_text, write_json, Activity, Attachment, Bind,
    CoreMsg, FrameError, Invoke, Say, TurnFailed, WireResponse,
};

/// gateway が invoke を実行する handler（DI 拡張 §5）。短縮参照(uN/eN/cN)は core が origin/pubkey へ
/// 解決済みで payload に入る。gateway は platform ID を導いて実行し、三結果のいずれかを返す。
#[async_trait::async_trait]
pub trait InvokeHandler: Send + Sync {
    async fn handle(&self, binding_id: &str, operation: &str, payload: &Value) -> InvokeOutcome;
}

/// invoke の三結果（§5.3）。gateway 側の観測を core へ正しく伝える。
pub enum InvokeOutcome {
    /// 外部 API が受理したと確認した。result は opaque JSON（null 含む）。
    Ok(Value),
    /// 外部 I/O 0 または確定非受理。`operation_rejected` を返す。
    Rejected,
    /// 受理成否が不明（timeout 等）。応答を作らず接続を閉じ、core 側で indeterminate に
    /// させる（不明を確定拒否へ捏造しない・§5.3）。
    Indeterminate,
}

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
        /// この say が**特定の inbound イベントへの返信**なら、その said の origin。
        ///
        /// 即時ターン（`occupy_until_turn_ends=true` の said 1 本）でだけ `Some`。bundle
        /// ターン（複数 said・activity started 起源）や、同一ターンに複数の即時 said が
        /// 相乗りした曖昧ケースでは `None`（＝単一の返信先が無い）。consumer（nostr-gateway）は
        /// `Some` を e-tag reply、`None` を「返信先無し」として扱う。web など返信先を使わない
        /// consumer は無視してよい。
        reply_origin: Option<String>,
    },
    Activity {
        activity_id: String,
        state: String,
        /// R2(👀): started が読み取ったターン発端の origin（state="started" のときだけ Some）。
        /// consumer（discord-gateway）は started+Some でこの origin へ 👀 を付ける。
        origin: Option<String>,
    },
    CompletedNoReply {
        /// 沈黙で終えたターン（say 無し）の発端 origin。即時ターン（`occupy_until_turn_ends`）が
        /// 単独で握った said（`ReplyOrigin::Single`）だけ `Some`。bundle ターンや複数即時 said の
        /// 相乗り（`None`/`Ambiguous`）では単一の発端を決められないので `None`。consumer は `Some` を
        /// 「その発端メッセージが沈黙で終えた」サイン（Discord なら 🤐）に使い、`None` は無視してよい。
        /// 裁定A（core が ended を say の後に出す）により、返信ターンでは saw_say=true のため
        /// このイベントは立たず、真の沈黙ターンだけに立つ。
        reply_origin: Option<String>,
    },
    /// R3(❌): ターン失敗（DeliveryEffect::Failed）。`reply_origin` は発端メッセージの origin。
    /// consumer（discord-gateway）はこの origin へ ❌ を付ける。error 本文は運ばない。
    TurnFailed { reply_origin: String },
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
    /// セッションキュー満杯。turn 実行中だけでは拒否しない。
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

/// 進行中ターンの返信先追跡。即時 said（`occupy_until_turn_ends`）が origin を刻む。
#[derive(Clone)]
enum ReplyOrigin {
    /// まだ said を刻んでいない（bundle ターンは activity started で None のまま生成される）。
    None,
    /// 即時 said 1 本だけが握ったターン。その said の origin。
    Single(String),
    /// 同一ターンに複数の即時 said が相乗り。単一の返信先を決められない。
    Ambiguous,
}

struct PendingTurn {
    saw_say: bool,
    reply_origin: ReplyOrigin,
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
    /// DI 拡張 §3.1: hello に載せる能力宣言配列（None は従来の hello＝能力ゼロ）。
    operations: Option<Value>,
    /// DI 拡張 §5: invoke の実行 handler（None は operation_unknown を返す）。
    invoke_handler: Option<Arc<dyn InvokeHandler>>,
    inner: Mutex<Inner>,
    write: Mutex<WriteOut>,
    closed_notify: Notify,
    req_seq: AtomicU64,
}

impl InstanceClient {
    fn blank(
        instance_id: String,
        author_id: String,
        say_policy: SayPolicy,
        operations: Option<Value>,
        invoke_handler: Option<Arc<dyn InvokeHandler>>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        Self {
            instance_id,
            author_id,
            say_policy,
            operations,
            invoke_handler,
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
            None,
            None,
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
        let client = Arc::new(Self::blank(instance_id, author_id, say_policy, None, None));
        tokio::spawn(reconnect_loop(
            client.clone(),
            socket,
            revision,
            config_digest,
        ));
        client
    }

    /// DI 能力宣言つきで接続する（nostr-gateway 等）。`operations` を hello に載せ、invoke は
    /// `invoke_handler` で実行する。従来の `spawn` は operations/handler なし（能力ゼロ）。
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_operations(
        socket: PathBuf,
        instance_id: String,
        revision: u64,
        author_id: String,
        config_digest: String,
        say_policy: SayPolicy,
        operations: Option<Value>,
        invoke_handler: Arc<dyn InvokeHandler>,
    ) -> Arc<Self> {
        let client = Arc::new(Self::blank(
            instance_id,
            author_id,
            say_policy,
            operations,
            Some(invoke_handler),
        ));
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

    /// Bundle member 用。ack までだけ `pending_turn` を残す。
    ///
    /// Accepted のあと turn が始まらない（coordinator が全 receipt 待ち）ときに
    /// 次の origin を送れる。ターン中の `CompletedNoReply` 追跡は activity started が立てる。
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
            let entry = inner
                .pending_turn
                .entry(binding_id.clone())
                .or_insert_with(|| PendingTurn {
                    saw_say: false,
                    reply_origin: ReplyOrigin::None,
                });
            // 即時 said（ターン終了まで占有）だけが返信先を刻む。bundle receipt
            // （occupy=false）は ack 後に pending_turn ごと消えるので刻まない。
            if occupy_until_turn_ends {
                entry.reply_origin = match &entry.reply_origin {
                    ReplyOrigin::None => ReplyOrigin::Single(origin.to_string()),
                    ReplyOrigin::Single(_) | ReplyOrigin::Ambiguous => ReplyOrigin::Ambiguous,
                };
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
        hello_frame_with_operations(
            &hello_id,
            &client.instance_id,
            revision,
            config_digest,
            client.operations.as_ref(),
        ),
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
        // fail-loud（#894）: サーバの err_frame コードをそのまま報告する。従来は WireErr の
        // code を捨て、後続 EOF を read_loop が `close_all("disconnect")` に潰していたため、
        // 真因（config_digest_mismatch 等）が両側で不可視だった。
        Ok(SaidOutcome::WireErr { code, detail }) => {
            tracing::warn!(
                instance_id = %client.instance_id,
                reason = %code,
                detail = ?detail,
                "hello failed"
            );
            Err(FrameError::Io)
        }
        Ok(SaidOutcome::Disconnected) => {
            tracing::warn!(
                instance_id = %client.instance_id,
                reason = "disconnect",
                "hello failed"
            );
            Err(FrameError::Io)
        }
        Err(_) => {
            tracing::warn!(
                instance_id = %client.instance_id,
                reason = "channel_closed",
                "hello failed"
            );
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
        CoreMsg::TurnFailed(tf) => {
            handle_turn_failed(client, tf).await;
            false
        }
        CoreMsg::Invoke(inv) => handle_invoke(client, inv, generation).await,
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

/// invoke を実行して応答する。戻り値は「接続を閉じるべきか」（Indeterminate のとき true）。
async fn handle_invoke(client: &InstanceClient, inv: Invoke, generation: u64) -> bool {
    tracing::info!(
        instance_id = %client.instance_id,
        binding_id = %inv.binding_id,
        operation = %inv.operation,
        "invoke"
    );
    // handler 未配線（能力ゼロ）なら未宣言 operation として fail-closed で operation_unknown
    // （外部 I/O 0・§5.1）。
    let Some(handler) = &client.invoke_handler else {
        let _ = send_frame(client, err_frame(&inv.id, "operation_unknown", None)).await;
        return false;
    };
    match handler
        .handle(&inv.binding_id, &inv.operation, &inv.payload)
        .await
    {
        InvokeOutcome::Ok(result) => {
            let _ = send_frame(client, invoke_ok_frame(&inv.id, &result)).await;
            false
        }
        InvokeOutcome::Rejected => {
            let _ = send_frame(client, err_frame(&inv.id, "operation_rejected", None)).await;
            false
        }
        InvokeOutcome::Indeterminate => {
            // 受理不明: 応答を作らず接続を閉じる。core は EOF を見て pending invoke を
            // indeterminate/disconnect にする（§5.3・不明を確定拒否へ捏造しない）。
            close_all(client, "invoke_indeterminate", generation).await;
            true
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
    // 返信先: payload の明示 reply_target（送信側が載せた発端 origin・resume 等）を最優先。
    // 無ければ進行中ターンの pending_turn（即時 said が刻んだ Single だけ Some）に委ねる。
    let reply_origin = say_reply_target(&say.payload)
        .map(str::to_string)
        .or_else(|| match inner.pending_turn.get(&say.binding_id) {
            Some(turn) => match &turn.reply_origin {
                ReplyOrigin::Single(o) => Some(o.clone()),
                ReplyOrigin::None | ReplyOrigin::Ambiguous => None,
            },
            None => None,
        });
    let q = inner
        .live
        .entry(address.clone())
        .or_insert_with(LiveQueue::new);
    let accepted = q.try_push(LiveEvent::Message { text, reply_origin });
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
        origin: activity.origin.clone(),
    });
    if activity.state == "started" {
        inner
            .pending_turn
            .entry(activity.binding_id.clone())
            .or_insert_with(|| PendingTurn {
                saw_say: false,
                reply_origin: ReplyOrigin::None,
            });
    } else if activity.state == "ended" {
        if let Some(turn) = inner.pending_turn.remove(&activity.binding_id) {
            if !turn.saw_say {
                // Message と同じ pending_turn.reply_origin を露出する（新フレームではなく既存追跡の
                // surface）。Single だけ発端を運び、None/Ambiguous は単一発端無しとして None。
                let reply_origin = match &turn.reply_origin {
                    ReplyOrigin::Single(o) => Some(o.clone()),
                    ReplyOrigin::None | ReplyOrigin::Ambiguous => None,
                };
                if let Some(q) = inner.live.get_mut(&address) {
                    let _ = q.try_push(LiveEvent::CompletedNoReply { reply_origin });
                }
            }
        }
    }
}

/// R3(❌): core→gate のターン失敗通知を live queue へ載せる。binding_id→address を解決し、
/// 未 ack binding は捨てる（handle_activity と同じ経路）。id を持たない通知なので応答は返さない。
async fn handle_turn_failed(client: &InstanceClient, tf: TurnFailed) {
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return;
    }
    let address = inner
        .acknowledged
        .iter()
        .find(|(_, bid)| *bid == &tf.binding_id)
        .map(|(a, _)| a.clone());
    let Some(address) = address else {
        return;
    };
    let q = inner.live.entry(address).or_insert_with(LiveQueue::new);
    let _ = q.try_push(LiveEvent::TurnFailed {
        reply_origin: tf.origin,
    });
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
