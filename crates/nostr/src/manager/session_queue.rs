use super::*;

/// 処理済み event.id の bounded FIFO セット（watch 再購読時の再処理 = 二重返信を防ぐ）。
pub(super) struct SeenEvents {
    order: std::collections::VecDeque<String>,
    set: std::collections::HashSet<String>,
    cap: usize,
}

impl SeenEvents {
    pub(super) fn new(cap: usize) -> Self {
        Self {
            order: std::collections::VecDeque::new(),
            set: std::collections::HashSet::new(),
            cap,
        }
    }

    /// 新規なら true を返して記録。既知なら false。
    pub(super) fn check_and_insert(&mut self, id: &str) -> bool {
        if self.set.contains(id) {
            return false;
        }
        self.set.insert(id.to_string());
        self.order.push_back(id.to_string());
        if self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

/// 応答生成 1 件ぶんの仕事（boxed future）。session キューを非ジェネリックに保つため
/// runner 型はここで消す。
pub(super) type ResponseJob = Pin<Box<dyn Future<Output = ()> + Send>>;

/// session ごとの inbound FIFO キュー束（順序保証 + 流量制限 / #168・#178）。
///
/// 受信ループは [`Self::enqueue`] を**同期的に**呼ぶだけ（`try_send` のみ・await 無し）。
/// session ごとに 1 本だけ走る consumer タスクがキューから FIFO で取り出し、permit を
/// 取ってから順に処理する。これで
///
/// - 同一セッションの処理順 = 投入順（連投の返信が入れ替わらない）
/// - 別セッションは並行（ある相手が詰まっても他の相手の受信は進む）
/// - ループは permit もロックも待たない（head-of-line blocking なし）
///
/// が同時に成り立つ。エントリの生成・回収と `try_send` / `try_recv` は同じ `queues`
/// ロックの下で行うので、「回収した直後の投入」で job を取りこぼさない。
pub(super) struct SessionQueues {
    capacity: usize,
    /// std Mutex: ガードの下では `try_send` / `try_recv` / map 操作しかせず await を跨がない。
    queues: Mutex<HashMap<String, mpsc::Sender<ResponseJob>>>,
    /// キュー溢れで捨てた件数（観測用）。
    dropped: AtomicU64,
}

impl SessionQueues {
    pub(super) fn new(capacity: usize) -> Self {
        debug_assert!(capacity >= 1, "session queue capacity must be >= 1");
        Self {
            capacity: capacity.max(1),
            queues: Mutex::new(HashMap::new()),
            dropped: AtomicU64::new(0),
        }
    }

    /// 走行中の session キュー数（回収の検証用）。
    #[cfg(test)]
    pub(super) fn active_sessions(&self) -> usize {
        self.queues.lock().unwrap().len()
    }

    /// キュー溢れで捨てた累計件数（本番はログで観測する）。
    #[cfg(test)]
    pub(super) fn dropped(&self) -> u64 {
        self.dropped.load(AtomicOrdering::SeqCst)
    }

    /// session のキューへ job を投入する。**ブロックしない**（`try_send` のみ）。
    pub(super) fn enqueue(
        self: &Arc<Self>,
        agent_id: &str,
        session_id: &str,
        permits: &Arc<Semaphore>,
        job: ResponseJob,
    ) {
        let mut queues = self.queues.lock().unwrap();
        // Sender は cheap clone。借用を切ってから try_send する（Closed 時に map を触るため）。
        let existing = queues.get(session_id).cloned();
        let job = match existing {
            Some(tx) => match tx.try_send(job) {
                Ok(()) => return,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    let dropped = self.dropped.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                    warn!(
                        agent_id,
                        session_id,
                        capacity = self.capacity,
                        dropped_total = dropped,
                        "nostr: セッションの受信キューが上限に達したため応答生成をスキップした（投稿本文は会話履歴に転記済み）"
                    );
                    return;
                }
                // consumer が消えているのにエントリが残っている（想定外）。作り直す。
                Err(mpsc::error::TrySendError::Closed(job)) => {
                    queues.remove(session_id);
                    job
                }
            },
            None => job,
        };
        self.spawn_consumer(&mut queues, agent_id, session_id, permits, job);
    }

    /// session の consumer タスクを起こす（`queues` ロック保持下で呼ぶ）。
    fn spawn_consumer(
        self: &Arc<Self>,
        queues: &mut HashMap<String, mpsc::Sender<ResponseJob>>,
        agent_id: &str,
        session_id: &str,
        permits: &Arc<Semaphore>,
        first: ResponseJob,
    ) {
        let (tx, rx) = mpsc::channel(self.capacity);
        // capacity >= 1 なので先頭 job は必ず入る。
        if tx.try_send(first).is_err() {
            error!(agent_id, session_id, "nostr: 受信キューの初期化に失敗した");
            return;
        }
        queues.insert(session_id.to_string(), tx);
        let this = self.clone();
        let permits = permits.clone();
        let agent_id = agent_id.to_string();
        let session_id = session_id.to_string();
        tokio::spawn(async move { this.run_consumer(rx, permits, agent_id, session_id).await });
    }

    /// session の consumer 本体。キューを FIFO で処理し、空になったら自分ごと回収する。
    async fn run_consumer(
        self: Arc<Self>,
        mut rx: mpsc::Receiver<ResponseJob>,
        permits: Arc<Semaphore>,
        agent_id: String,
        session_id: String,
    ) {
        loop {
            let job = match rx.try_recv() {
                Ok(job) => job,
                // 空に見えた → ロック下で再確認し、本当に空ならエントリを回収して終わる。
                Err(_) => match self.retire_or_take(&session_id, &mut rx) {
                    Some(job) => job,
                    None => {
                        debug!(
                            agent_id,
                            session_id, "nostr: アイドルな session キューを回収した"
                        );
                        return;
                    }
                },
            };
            // #665: キューから job を取り出した（この session の次のターンが動き出す）。相関キーは
            // session_id（ターンはこの consumer で FIFO 直列化される）。turn_id は run_agent_response で採番。
            debug!(
                agent_id,
                session_id,
                stage = "nostr_consumer",
                "turn: consumer job 取り出し"
            );
            // 流量制限は **ここ**（ループ外）で取る。受信ループ側で取ると、session ロック
            // 待ちで何もしていないタスクが permit を占有してループ全体が止まる。
            // #665: permit 取得は宙吊り候補（全 permit を他ターンが握ると後続はここで待つ）。入と出を出す。
            debug!(
                agent_id,
                session_id,
                stage = "nostr_consumer",
                "turn: 応答 permit 取得待ち（入）"
            );
            let Ok(_permit) = permits.clone().acquire_owned().await else {
                self.queues.lock().unwrap().remove(&session_id);
                warn!(
                    agent_id,
                    session_id, "nostr: 応答 semaphore が閉じたので session consumer を終了する"
                );
                return;
            };
            debug!(
                agent_id,
                session_id,
                stage = "nostr_consumer",
                "turn: 応答 permit 取得（出）"
            );
            // #665: ターン job 本体の実行。この後の `job.await`（＝respond_serialized→run→engine）が
            // 返らなければ、後続 job はキューで待ち続け「ターン開始」ログすら出ない（実観測の宙吊り像）。
            debug!(
                agent_id,
                session_id,
                stage = "nostr_consumer",
                "turn: ターン job 実行開始（入）"
            );
            job.await;
            debug!(
                agent_id,
                session_id,
                stage = "nostr_consumer",
                "turn: ターン job 完了（出）"
            );
        }
    }

    /// キューが空なら map エントリを回収して `None`、新着があればその job を返す。
    ///
    /// 判定を `queues` ロックの下で行うことが要点。[`Self::enqueue`] も同じロックの下で
    /// `try_send` するので、「空と判定 → 回収」の隙間に投入が挟まることがない。
    fn retire_or_take(
        &self,
        session_id: &str,
        rx: &mut mpsc::Receiver<ResponseJob>,
    ) -> Option<ResponseJob> {
        let mut queues = self.queues.lock().unwrap();
        match rx.try_recv() {
            Ok(job) => Some(job),
            Err(_) => {
                queues.remove(session_id);
                None
            }
        }
    }
}

/// scheduler の時刻発火（#588 TimedFire）を Nostr のセッションキューへ流す受け口。
///
/// [`opencrab_actions::TimedFireRouter`] に登録され、`fire_timed_turn` で「その回のプロンプトで
/// 1 ターン回す」ジョブを per-agent キューへ enqueue するだけ（受け口は薄く保つ）。以降は Nostr の
/// 既存経路（[`NostrResponder`]・直列化・継続ターンの SubtaskCompletionSink）が回す。
///
/// **ブロードキャストなので reply_target は空**。Nostr はもともと機構が publish しない（配送は
/// エージェントが `nostr_post` 等のツールで自分から行う・#588）ので、時刻発火の特別扱いは要らない:
/// 応答はセッションへ転記され（返信先が無いのでアンカー無し）、外界へは出ない。
pub(super) struct NostrTimedFireSink<R: NostrAgentRunner> {
    pub(super) runner: R,
    pub(super) cli: NostaroCli,
    pub(super) runtime: Arc<NostrSessionRuntime>,
    pub(super) admin: Arc<dyn NostrIdentityAdmin>,
    pub(super) queues: Arc<SessionQueues>,
    pub(super) permits: Arc<Semaphore>,
}

impl<R: NostrAgentRunner + Clone> opencrab_actions::TimedFireSink for NostrTimedFireSink<R> {
    fn fire_timed_turn(&self, req: opencrab_actions::TimedFireRequest) {
        let responder = NostrResponder::new(
            self.runner.clone(),
            self.cli.clone(),
            self.runtime.clone(),
            self.admin.clone(),
            req.agent_id.clone(),
        );
        let session_id = req.session_id.clone();
        let prompt = req.prompt;
        let caller = req.caller;
        let agent_id_for_log = req.agent_id.clone();
        let prompt_preview = opencrab_actions::prompt_preview(&prompt);
        let job: ResponseJob = Box::pin(async move {
            // 時刻発火の受信ログ（#588）: この Nostr ループのキューからジョブが取り出されターンが
            // 始まった。送信側（scheduler）の「発火」ログと突き合わせれば scheduler→ループ間で
            // 落ちたかが分かる。heartbeat 専用の文言にしない（アラーム・定時実行も同じイベント）。
            info!(
                agent_id = %agent_id_for_log,
                session_id = %session_id,
                transport = "nostr",
                prompt_preview = %prompt_preview,
                "timed-fire: ターン開始（Nostr loop 受信）"
            );
            // reply_target は空（ブロードキャスト）。機構は publish しないので特別扱い不要。応答は
            // セッションへ転記され、外界へはエージェントがツールで出す（#588）。
            responder
                .respond_serialized(
                    &session_id,
                    "",
                    &prompt,
                    None,
                    caller,
                    opencrab_actions::LiveInboundScope::AllOthers,
                )
                .await;
        });
        // #665: 時刻発火をこの Nostr ループのキューへ投入する（fire-and-forget）。発火側の「発火
        // （trigger → gateway loop）」ログとこの行が session_id で繋がる。この後 consumer が job を
        // 取り出すまでの間に詰まると「ターン開始（Nostr loop 受信）」が出ないまま沈黙する（実観測像）。
        debug!(
            agent_id = %req.agent_id,
            session_id = %req.session_id,
            stage = "nostr_sink",
            "turn: 時刻発火を Nostr キューへ投入"
        );
        self.queues
            .enqueue(&req.agent_id, &req.session_id, &self.permits, job);
    }
}
