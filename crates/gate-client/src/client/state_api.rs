/// gateway が invoke を実行する handler（DI 拡張 §5）。短縮参照(uN/eN/cN)は core が origin/pubkey へ
/// 解決済みで payload に入る。gateway は platform ID を導いて実行し、三結果のいずれかを返す。
/// The connection owns this future and drops it on close. Keep external effects inside the
/// future (not detached tasks); cancellation cannot undo effects already transmitted.
#[async_trait::async_trait]
pub trait InvokeHandler: Send + Sync {
    async fn handle(
        &self,
        call_id: &str,
        session_key: &str,
        operation: &str,
        payload: &Value,
    ) -> InvokeOutcome;
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

#[derive(Debug)]
pub enum SaidOutcome {
    Accepted {
        seq: i64,
    },
    /// D-1099: `open` answered with the key's core session.
    Opened {
        session_id: String,
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
pub enum OpenError {
    NotReady,
    Rejected { code: String },
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    NotReady,
    Rejected {
        code: String,
        message: Option<String>,
    },
    Timeout,
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
    Said,
    Open,
}

struct PendingSaid {
    kind: PendingKind,
    reply: oneshot::Sender<SaidOutcome>,
}

struct PendingCommand {
    reply: oneshot::Sender<Result<Value, CommandError>>,
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
        self.try_push_batch(vec![ev])
    }

    /// One wire frame may expand into ended, completed, and multiple silent outcomes. Reserve the
    /// whole batch before making any item observable so capacity pressure cannot expose a prefix.
    fn try_push_batch(&mut self, events: Vec<LiveEvent>) -> bool {
        if events.len() > LIVE_QUEUE_CAP.saturating_sub(self.events.len()) {
            return false;
        }
        self.events.extend(events);
        while !self.events.is_empty() {
            let Some(idx) = self.waiters.iter().position(|waiter| !waiter.is_closed()) else {
                break;
            };
            let waiter = self.waiters.remove(idx);
            let event = self.events.pop_front().expect("queue checked non-empty");
            if let Err(event) = waiter.send(event) {
                self.events.push_front(event);
            }
        }
        true
    }

    fn subscribe(&mut self) -> Option<LiveEvent> {
        self.events.pop_front()
    }
}

struct Inner {
    pending_said: HashMap<String, PendingSaid>,
    pending_commands: HashMap<String, PendingCommand>,
    expired_commands: HashSet<String>,
    pending_turn: HashMap<String, PendingTurn>,
    live: HashMap<String, LiveQueue>,
    closed: bool,
    generation: u64,
    stop: Option<tokio::sync::watch::Sender<()>>,
}

pub struct InstanceClient {
    pub instance_id: String,
    /// D-1099: the agent this instance speaks for (sent in `hello`).
    pub agent_id: String,
    pub author_id: String,
    say_policy: SayPolicy,
    /// DI 拡張 §3.1: hello に載せる能力宣言配列（None は従来の hello＝能力ゼロ）。
    operations: Option<Value>,
    runtime_capabilities: super::wire::RuntimeCapabilities,
    /// D-1091: hello に載せる、この instance の binding 用の環境変数（名前→値）。core は解釈せず
    /// `execute_shell` の子へ渡すだけ。空なら従来と同じ frame。
    run_env: std::collections::BTreeMap<String, String>,
    /// DI 拡張 §5: invoke の実行 handler（None は operation_unknown を返す）。
    invoke_handler: Option<Arc<dyn InvokeHandler>>,
    inner: Mutex<Inner>,
    write: Mutex<WriteOut>,
    closed_notify: Notify,
    connection: Mutex<Option<tokio::task::JoinHandle<()>>>,
    req_seq: AtomicU64,
}

impl InstanceClient {
    fn blank(
        instance_id: String,
        agent_id: String,
        author_id: String,
        say_policy: SayPolicy,
        operations: Option<Value>,
        runtime_capabilities: super::wire::RuntimeCapabilities,
        invoke_handler: Option<Arc<dyn InvokeHandler>>,
    ) -> Self {
        Self::blank_with_env(
            instance_id,
            agent_id,
            author_id,
            say_policy,
            operations,
            runtime_capabilities,
            std::collections::BTreeMap::new(),
            invoke_handler,
        )
    }

    /// D-1091: [`Self::blank`] plus the environment the gateway declares for this instance.
    fn blank_with_env(
        instance_id: String,
        agent_id: String,
        author_id: String,
        say_policy: SayPolicy,
        operations: Option<Value>,
        runtime_capabilities: super::wire::RuntimeCapabilities,
        run_env: std::collections::BTreeMap<String, String>,
        invoke_handler: Option<Arc<dyn InvokeHandler>>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        Self {
            instance_id,
            agent_id,
            author_id,
            say_policy,
            operations,
            runtime_capabilities,
            run_env,
            invoke_handler,
            inner: Mutex::new(Inner {
                pending_said: HashMap::new(),
                pending_commands: HashMap::new(),
                expired_commands: HashSet::new(),
                pending_turn: HashMap::new(),
                live: HashMap::new(),
                closed: true,
                generation: 0,
                stop: None,
            }),
            write: Mutex::new(WriteOut { tx }),
            closed_notify: Notify::new(),
            connection: Mutex::new(None),
            req_seq: AtomicU64::new(1),
        }
    }

    /// 1 回だけ connect。切断後の再接続はしない（conformance 用）。
    pub async fn connect(
        socket: &std::path::Path,
        instance_id: String,
        agent_id: String,
        author_id: String,
    ) -> Result<Arc<Self>, FrameError> {
        let client = Arc::new(Self::blank(
            instance_id,
            agent_id,
            author_id,
            SayPolicy::AcceptToLiveQueue,
            None,
            super::wire::RuntimeCapabilities::default(),
            None,
        ));
        attach(&client, socket).await?;
        Ok(client)
    }

    /// HTTP を先に生かし、UDS は指数 backoff でつなぎ続ける。
    pub fn spawn(
        socket: PathBuf,
        instance_id: String,
        agent_id: String,
        author_id: String,
    ) -> Arc<Self> {
        Self::spawn_with_say_policy(
            socket,
            instance_id,
            agent_id,
            author_id,
            SayPolicy::AcceptToLiveQueue,
        )
    }

    pub fn spawn_with_say_policy(
        socket: PathBuf,
        instance_id: String,
        agent_id: String,
        author_id: String,
        say_policy: SayPolicy,
    ) -> Arc<Self> {
        let client = Arc::new(Self::blank(
            instance_id,
            agent_id,
            author_id,
            say_policy,
            None,
            super::wire::RuntimeCapabilities::default(),
            None,
        ));
        tokio::spawn(reconnect_loop(client.clone(), socket));
        client
    }

    /// DI能力宣言つきで接続する。`operations`をhelloに載せ、invokeは
    /// `invoke_handler` で実行する。従来の `spawn` は operations/handler なし（能力ゼロ）。
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_operations(
        socket: PathBuf,
        instance_id: String,
        agent_id: String,
        author_id: String,
        say_policy: SayPolicy,
        operations: Option<Value>,
        runtime_capabilities: super::wire::RuntimeCapabilities,
        invoke_handler: Arc<dyn InvokeHandler>,
    ) -> Arc<Self> {
        let client = Arc::new(Self::blank(
            instance_id,
            agent_id,
            author_id,
            say_policy,
            operations,
            runtime_capabilities,
            Some(invoke_handler),
        ));
        tokio::spawn(reconnect_loop(client.clone(), socket));
        client
    }

    /// D-1091: `spawn_with_operations` plus the environment this gateway declares for the
    /// instance. Core hands it to `execute_shell` children of that instance's turns.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_operations_and_env(
        socket: PathBuf,
        instance_id: String,
        agent_id: String,
        author_id: String,
        say_policy: SayPolicy,
        operations: Option<Value>,
        runtime_capabilities: super::wire::RuntimeCapabilities,
        invoke_handler: Arc<dyn InvokeHandler>,
        run_env: std::collections::BTreeMap<String, String>,
    ) -> Arc<Self> {
        let client = Arc::new(Self::blank_with_env(
            instance_id,
            agent_id,
            author_id,
            say_policy,
            operations,
            runtime_capabilities,
            run_env,
            Some(invoke_handler),
        ));
        tokio::spawn(reconnect_loop(client.clone(), socket));
        client
    }

    pub async fn connection_live(&self) -> bool {
        !self.inner.lock().await.closed
    }

    fn next_id(&self) -> String {
        let n = self.req_seq.fetch_add(1, Ordering::Relaxed);
        format!("said:{n}")
    }

    /// Validate an invoke against the immutable hello snapshot before calling the adapter handler.
    pub(super) fn validate_invocation(&self, invoke: &super::wire::Invoke) -> Result<(), &'static str> {
        let operations = self
            .operations
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .ok_or("operation_unknown")?;
        let declaration = operations
            .iter()
            .find(|entry| entry.get("name").and_then(serde_json::Value::as_str) == Some(&invoke.operation))
            .ok_or("operation_unknown")?;
        let digest = super::wire::runtime_declaration_digest_from_value(
            self.operations.as_ref(),
            self.runtime_capabilities,
        )
        .ok_or("operation_rejected")?;
        if invoke.declaration_digest != digest
            || declaration.get("dispatch").and_then(serde_json::Value::as_str)
                != Some(&invoke.dispatch)
            || declaration.get("effect").and_then(serde_json::Value::as_str)
                != Some(&invoke.effect)
        {
            return Err("operation_rejected");
        }
        Ok(())
    }

    /// D-1099: the core session of `session_key`, created when new (`open`). Gateways that
    /// show a conversation before its first said use it; others never need it.
    pub async fn open(&self, session_key: &str, title: Option<&str>) -> Result<String, OpenError> {
        if self.inner.lock().await.closed {
            return Err(OpenError::NotReady);
        }
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        self.inner.lock().await.pending_said.insert(
            id.clone(),
            PendingSaid {
                kind: PendingKind::Open,
                reply: tx,
            },
        );
        if !send_frame(self, open_frame(&id, session_key, title)).await {
            self.inner.lock().await.pending_said.remove(&id);
            return Err(OpenError::Disconnected);
        }
        match tokio::time::timeout(SAID_TIMEOUT, rx).await {
            Ok(Ok(SaidOutcome::Opened { session_id })) => Ok(session_id),
            Ok(Ok(SaidOutcome::WireErr { code, .. })) => Err(OpenError::Rejected { code }),
            Ok(Ok(_)) | Ok(Err(_)) => Err(OpenError::Disconnected),
            Err(_) => {
                self.inner.lock().await.pending_said.remove(&id);
                Err(OpenError::Disconnected)
            }
        }
    }

    /// Send one generic command over the current connection. The caller owns the timeout; timed
    /// out correlations are removed and a late response is ignored.
    pub async fn command(
        &self,
        address: &str,
        caller: &SaidCaller,
        name: &str,
        args: &Value,
        timeout: Duration,
    ) -> Result<Value, CommandError> {
        if !args.is_object() {
            return Err(CommandError::Rejected {
                code: "invalid_args".to_string(),
                message: Some("Invalid command arguments.".to_string()),
            });
        }
        if self.inner.lock().await.closed {
            return Err(CommandError::NotReady);
        }
        let id = format!("command:{}", self.req_seq.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.inner
            .lock()
            .await
            .pending_commands
            .insert(id.clone(), PendingCommand { reply: tx });
        if !send_frame(
            self,
            command_frame(&id, address, caller, name, args),
        )
        .await
        {
            self.inner.lock().await.pending_commands.remove(&id);
            return Err(CommandError::Disconnected);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(CommandError::Disconnected),
            Err(_) => {
                let mut inner = self.inner.lock().await;
                if inner.pending_commands.remove(&id).is_some() {
                    inner.expired_commands.insert(id);
                }
                Err(CommandError::Timeout)
            }
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
        {
            let mut inner = self.inner.lock().await;
            if inner.closed {
                return Err(PostRefuse::NotReady);
            }
            inner
                .live
                .entry(address.to_string())
                .or_insert_with(LiveQueue::new);
            let entry = inner
                .pending_turn
                .entry(address.to_string())
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
        }
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
        let frame = said_frame_with_context(
            &id,
            address,
            origin,
            author_id,
            author_label,
            context,
            text,
            attachments,
        );
        tracing::info!(
            instance_id = %self.instance_id,
            session = %address,
            origin,
            "said"
        );
        if !send_frame(self, frame).await {
            let mut inner = self.inner.lock().await;
            inner.pending_turn.remove(address);
            inner.pending_said.remove(&id);
            return Ok(SaidOutcome::Disconnected);
        }
        match tokio::time::timeout(SAID_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => {
                if !occupy_until_turn_ends || !matches!(outcome, SaidOutcome::Accepted { .. }) {
                    let mut inner = self.inner.lock().await;
                    inner.pending_turn.remove(address);
                }
                tracing::info!(
                    instance_id = %self.instance_id,
                    session = %address,
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

