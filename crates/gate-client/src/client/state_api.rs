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
    /// 外部側の沈黙表現を立てない。resolve/follow等の照会・操作classは既定の`false`。
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
        /// 相乗りした曖昧ケースでは`None`（単一の返信先が無い）。consumerは`Some`を
        /// 対象返信、`None`を「返信先無し」として扱う。返信先を使わないconsumerは無視してよい。
        reply_origin: Option<String>,
    },
    Activity {
        activity_id: String,
        state: String,
        /// #964: 次の LLM request に新しく含める投稿の origin（state="read" のときだけ Some）。
        /// consumerはread+Someを外部側の既読表現へ変換できる。
        origin: Option<String>,
    },
    /// #915: activity ended で core が指定した完了サインの付け先（発話 id）。
    Completed { target: String },
    CompletedNoReply {
        /// 沈黙で終えたターン（say 無し）の発端 origin。即時ターンなら、そのターンを受理した
        /// said の origin。bundle ターンには単一の発端がないため `None`。consumer は `Some` を
        /// 「その発端messageが沈黙で終えた」サインに使い、`None`は無視してよい。
        /// 裁定A（core が ended を say の後に出す）により、返信ターンでは saw_utterance=true のため
        /// このイベントは立たず、真の沈黙ターンだけに立つ。
        reply_origin: Option<String>,
    },
    /// R3(❌): ターン失敗（DeliveryEffect::Failed）。`reply_origin` は発端メッセージの origin。
    /// consumerはこのoriginへ外部側の失敗表現を付けられる。error本文は運ばない。
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateBindingError {
    NotReady,
    Rejected { code: String },
    Disconnected,
}

/// core からの `say` をどう扱うか。外部出力をlive queueへ受理するか拒否する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SayPolicy {
    AcceptToLiveQueue,
    RejectExternal,
}

enum PendingKind {
    Hello,
    Command,
    Said {
        reservation: Option<(String, String)>,
    },
}

struct PendingSaid {
    kind: PendingKind,
    reply: oneshot::Sender<SaidOutcome>,
}

/// 受理済みターンの返信先追跡。coreは同一bindingの受理済みimmediate saidを順に実行し、
/// activity endedもその受理順で送るため、gatewayは同じFIFO順でoriginを所有する。
struct PendingTurn {
    saw_utterance: bool,
    /// 即時saidは発端origin、bundle turnは単一発端がないためNone。
    reply_origin: Option<String>,
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
    pending_turns: HashMap<String, VecDeque<PendingTurn>>,
    live: HashMap<String, LiveQueue>,
    closed: bool,
    generation: u64,
}

fn remove_pending_origin(inner: &mut Inner, binding_id: &str, origin: &str) {
    let should_remove = if let Some(turns) = inner.pending_turns.get_mut(binding_id) {
        if let Some(position) = turns
            .iter()
            .rposition(|turn| turn.reply_origin.as_deref() == Some(origin))
        {
            turns.remove(position);
        }
        turns.is_empty()
    } else {
        false
    };
    if should_remove {
        inner.pending_turns.remove(binding_id);
    }
}

fn pop_pending_turn(inner: &mut Inner, binding_id: &str) -> Option<PendingTurn> {
    let (turn, empty) = {
        let turns = inner.pending_turns.get_mut(binding_id)?;
        let turn = turns.pop_front();
        (turn, turns.is_empty())
    };
    if empty {
        inner.pending_turns.remove(binding_id);
    }
    turn
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
                pending_turns: HashMap::new(),
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

    /// DI能力宣言つきで接続する。`operations`をhelloに載せ、invokeは
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

    /// 接続中instance自身のopaque bindingをcoreへ作成要求する。
    pub async fn create_binding(
        &self,
        binding_id: &str,
        address: &str,
        session_theme: &str,
    ) -> Result<(), CreateBindingError> {
        if self.inner.lock().await.closed {
            return Err(CreateBindingError::NotReady);
        }
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        self.inner.lock().await.pending_said.insert(
            id.clone(),
            PendingSaid {
                kind: PendingKind::Command,
                reply: tx,
            },
        );
        if !send_frame(
            self,
            create_binding_frame(&id, binding_id, address, session_theme),
        )
        .await
        {
            self.inner.lock().await.pending_said.remove(&id);
            return Err(CreateBindingError::Disconnected);
        }
        match tokio::time::timeout(SAID_TIMEOUT, rx).await {
            Ok(Ok(SaidOutcome::Accepted { .. })) => {
                let mut inner = self.inner.lock().await;
                match inner.remembered.get(address) {
                    Some(existing) if existing != binding_id => {
                        Err(CreateBindingError::Rejected {
                            code: "binding_conflict".to_string(),
                        })
                    }
                    Some(_) => Ok(()),
                    None => {
                        inner
                            .remembered
                            .insert(address.to_string(), binding_id.to_string());
                        Ok(())
                    }
                }
            }
            Ok(Ok(SaidOutcome::WireErr { code, .. })) => {
                Err(CreateBindingError::Rejected { code })
            }
            Ok(Ok(SaidOutcome::Disconnected)) | Ok(Err(_)) | Err(_) => {
                Err(CreateBindingError::Disconnected)
            }
            Ok(Ok(SaidOutcome::NotAdmitted)) => Err(CreateBindingError::Disconnected),
        }
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
            None,
            text,
            attachments,
            true,
        )
        .await
    }

    /// Platform-neutral contextを伴うSaid。個別gatewayの判断結果はこの汎用形で渡す。
    pub async fn post_said_with_self_context(
        &self,
        address: &str,
        origin: &str,
        context: &SaidContext,
        text: &str,
        attachments: &[Attachment],
    ) -> Result<SaidOutcome, PostRefuse> {
        self.post_said_with_context(
            address,
            origin,
            &self.author_id,
            None,
            context,
            text,
            attachments,
        )
        .await
    }

    /// Platform-neutral context with an explicitly authenticated author.
    #[allow(clippy::too_many_arguments)]
    pub async fn post_said_with_context(
        &self,
        address: &str,
        origin: &str,
        author_id: &str,
        author_label: Option<&str>,
        context: &SaidContext,
        text: &str,
        attachments: &[Attachment],
    ) -> Result<SaidOutcome, PostRefuse> {
        self.post_said_inner(
            address,
            origin,
            author_id,
            author_label,
            Some(context),
            text,
            attachments,
            true,
        )
        .await
    }

    /// Bundle member 用。bundle receipt自身はturn originを予約しない。
    ///
    /// Accepted のあとturnが始まらない（coordinatorが全receipt待ち）ときにも
    /// 次のoriginを送れる。ターン中の`CompletedNoReply`追跡はactivity startedが立てる。
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
            None,
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
        context: Option<&SaidContext>,
        text: &str,
        attachments: &[Attachment],
        occupy_until_turn_ends: bool,
    ) -> Result<SaidOutcome, PostRefuse> {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        // Acquire the current writer before reserving origin ownership. After reservation, queueing
        // the frame is synchronous, so task cancellation cannot leave an unsent reservation behind.
        let write = self.write.lock().await;
        let binding_id = {
            let mut inner = self.inner.lock().await;
            if inner.closed {
                return Err(PostRefuse::NotReady);
            }
            let Some(binding_id) = inner.acknowledged.get(address).cloned() else {
                return Err(PostRefuse::NotReady);
            };
            // Immediate said values reserve one turn each in wire-send order. Bundle receipts do
            // not own an origin; activity started creates their origin-less turn when needed.
            let reservation = occupy_until_turn_ends.then(|| {
                inner
                    .pending_turns
                    .entry(binding_id.clone())
                    .or_default()
                    .push_back(PendingTurn {
                        saw_utterance: false,
                        reply_origin: Some(origin.to_string()),
                    });
                (binding_id.clone(), origin.to_string())
            });
            inner.pending_said.insert(
                id.clone(),
                PendingSaid {
                    kind: PendingKind::Said { reservation },
                    reply: tx,
                },
            );
            binding_id
        };
        let frame = said_frame_with_context(
            &id,
            &binding_id,
            origin,
            author_id,
            author_label,
            context,
            text,
            attachments,
        );
        tracing::info!(
            instance_id = %self.instance_id,
            binding_id = %binding_id,
            origin,
            "said"
        );
        if write.tx.send(frame).is_err() {
            drop(write);
            let mut inner = self.inner.lock().await;
            if occupy_until_turn_ends {
                remove_pending_origin(&mut inner, &binding_id, origin);
            }
            inner.pending_said.remove(&id);
            return Ok(SaidOutcome::Disconnected);
        }
        drop(write);
        match tokio::time::timeout(SAID_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => {
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

