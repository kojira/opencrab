/// gateway が invoke を実行する handler（DI 拡張 §5）。短縮参照(uN/eN/cN)は core が origin/pubkey へ
/// 解決済みで payload に入る。gateway は platform ID を導いて実行し、三結果のいずれかを返す。
#[async_trait::async_trait]
pub trait InvokeHandler: Send + Sync {
    async fn handle(
        &self,
        call_id: &str,
        binding_id: &str,
        operation: &str,
        payload: &Value,
    ) -> InvokeOutcome;

    /// この operation が**発話クラス**（reply/reaction/repost 等・ユーザーに見える発言）か。
    ///
    /// #900: 発話は say と同じく「そのターンで発話した」証跡になる。gateway 固有の operation
    /// 名を知るのは handler なので、発話クラスの判定は handler が担う（gate-client は非依存）。
    /// これが `true` の invoke が Ok で決着すると、ターンは沈黙ではなくなり `CompletedNoReply`
    /// （Discord なら 🤐）を立てない。resolve/follow 等の照会・操作クラスは既定の `false`。
    fn is_utterance(&self, operation: &str) -> bool {
        let _ = operation;
        false
    }
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
        /// core が say に載せた既存 delivery id。gateway は platform message id との対応に使う。
        delivery_id: String,
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
        /// #964: 次の LLM request に新しく含める投稿の origin（state="read" のときだけ Some）。
        /// consumer（discord-gateway）は read+Some でこの origin へ 👀 を付ける。
        origin: Option<String>,
    },
    /// #915: activity ended で core が指定した完了サインの付け先（発話 id）。
    Completed { target: String },
    CompletedNoReply {
        /// 沈黙で終えたターン（say 無し）の発端 origin。即時ターン（`occupy_until_turn_ends`）が
        /// 単独で握った said（`ReplyOrigin::Single`）だけ `Some`。bundle ターンや複数即時 said の
        /// 相乗り（`None`/`Ambiguous`）では単一の発端を決められないので `None`。consumer は `Some` を
        /// 「その発端メッセージが沈黙で終えた」サイン（Discord なら 🤐）に使い、`None` は無視してよい。
        /// 裁定A（core が ended を say の後に出す）により、返信ターンでは saw_utterance=true のため
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
    saw_utterance: bool,
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
        self.post_said_with_author_label(
            address,
            origin,
            author_id,
            None,
            text,
            attachments,
        )
        .await
    }

    pub async fn post_said_with_author_label(
        &self,
        address: &str,
        origin: &str,
        author_id: &str,
        author_label: Option<&str>,
        text: &str,
        attachments: &[Attachment],
    ) -> Result<SaidOutcome, PostRefuse> {
        self.post_said_inner(
            address,
            origin,
            author_id,
            author_label,
            text,
            attachments,
            true,
        )
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
        self.post_said_receipt_with_author_label(
            address,
            origin,
            author_id,
            None,
            text,
            attachments,
        )
        .await
    }

    pub async fn post_said_receipt_with_author_label(
        &self,
        address: &str,
        origin: &str,
        author_id: &str,
        author_label: Option<&str>,
        text: &str,
        attachments: &[Attachment],
    ) -> Result<SaidOutcome, PostRefuse> {
        self.post_said_inner(
            address,
            origin,
            author_id,
            author_label,
            text,
            attachments,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn post_said_inner(
        &self,
        address: &str,
        origin: &str,
        author_id: &str,
        author_label: Option<&str>,
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
                    saw_utterance: false,
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
        let frame = said_frame_with_author_label(
            &id,
            &binding_id,
            origin,
            author_id,
            author_label,
            text,
            attachments,
        );
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

