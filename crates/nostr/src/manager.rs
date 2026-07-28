//! Per-agent Nostr sub-gateway マネージャ + watch ループ。
//!
//! Discord の `DiscordGatewayManager` と同型。エージェント毎に nostaro の `watch --json`
//! を spawn し、JSONL イベントを読んで `run_agent_response` → 返信する。
//!
//! **受信ループは応答生成でブロックしない**（#178）。応答生成（会話再構築 → LLM →
//! 返信）は受信ループの外へ出し、ループは即次の行へ進む。
//!
//! ただし単純に `tokio::spawn` へ投げると、**同一相手からの連投の処理順が
//! 「どの spawn タスクが先に session ロックを取るか」で決まる**（= ランダム）。
//! 5 通目への返信が 1 通目より先に届きうる。そこで [`SessionQueues`] を挟み、
//! **session ごとに 1 本の consumer タスク**が bounded な mpsc から FIFO で
//! 取り出して処理する（per-session 直列 + 順序保証、別セッションは並行）。
//! consumer はキューが空になったら自分ごと回収される（task/チャネルのリーク防止）。
//!
//! 同時実行上限（[`MAX_CONCURRENT_RESPONSES`]）の permit は **consumer タスクの内側**
//! で取る。受信ループ側で取ると「session ロック待ちで何もしていないタスク」が permit を
//! 占有し、上限が埋まった時点でループ全体（＝全相手の受信）が止まる（head-of-line
//! blocking / #178 が直そうとしたバグと同型）。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use anyhow::Context;

use crate::cli::NostaroCli;
use crate::config::NostrConfig;
use crate::event::{parse_watch_line, NostrEvent};
use crate::identity::NostrIdentityAdmin;
use crate::runner::NostrAgentRunner;
use crate::session::{nostr_session_id, NostrSessionRuntime};
use crate::sink::NostrResponder;

/// watch ループが握る self_pubkey の共有セル（identity 切替で更新可能）。
type SelfPubkey = Arc<RwLock<String>>;

/// watch が落ちたときの再接続バックオフ。
const WATCH_RESTART_DELAY: Duration = Duration::from_secs(5);

/// 応答生成の同時実行上限（per-agent / #178）。
///
/// 受信ループを塞がないために応答生成はループ外で走らせるが、無制限に走らせると洪水時に
/// LLM 呼び出しとメモリが暴走する。permit で「同時に走る応答生成は最大 N 本」に絞る。
/// permit の取得は **consumer タスクの内側**（[`SessionQueues::run_consumer`]）で行う。
/// 受信ループ側で取ると待機中のタスクが permit を占有してループが止まる。
const MAX_CONCURRENT_RESPONSES: usize = 8;

/// per-session の inbound キュー容量（per-agent / #168）。
///
/// 応答生成は LLM 1 往復ぶんかかるので、1 人の相手が連投し続けるとキューは伸びる。
/// 無制限に伸ばすとメモリと「もう誰も待っていない返信」が溜まるだけなので上限を置き、
/// 溢れたぶんは**ログに残して**捨てる（本文は転記済みなので次の応答の会話履歴に載る）。
const SESSION_QUEUE_CAPACITY: usize = 32;

pub struct NostrGatewayManager<R: NostrAgentRunner> {
    // std RwLock: is_running を同期メソッドにするため。ガードは await を跨がない。
    gateways: RwLock<HashMap<String, JoinHandle<()>>>,
    runner: R,
    cli: NostaroCli,
    /// per-session 直列化ロック + dispatch registry（#168）。全エージェント横断で 1 つ。
    /// watch ループと完了 sink が同じ Arc を共有することが、二重投稿の防止
    /// （直列化）と `cancel_subtask` 到達性（同一 registry）の条件。
    runtime: Arc<NostrSessionRuntime>,
}

impl<R: NostrAgentRunner> NostrGatewayManager<R> {
    pub fn new(runner: R) -> Self {
        Self {
            gateways: RwLock::new(HashMap::new()),
            runner,
            cli: NostaroCli::new(),
            runtime: Arc::new(NostrSessionRuntime::new()),
        }
    }

    pub fn with_cli(mut self, cli: NostaroCli) -> Self {
        self.cli = cli;
        self
    }

    /// per-session ランタイム（直列化ロック + dispatch registry）。
    pub fn session_runtime(&self) -> &Arc<NostrSessionRuntime> {
        &self.runtime
    }

    /// エージェントの Nostr ゲートウェイを起動する。
    ///
    /// 秘密鍵/リレーから per-agent config.toml を materialize（0600）し、自分の pubkey を
    /// 取得（自己返信ループ防止）して watch ループを spawn する。
    pub async fn start_agent_gateway(
        &self,
        agent_id: &str,
        secret_key: &str,
        config: NostrConfig,
    ) -> anyhow::Result<()> {
        // 資格情報のガード（#191 段階2 PR3）。空 / 空白だけの nsec では nostaro が
        // 動かないので、materialize（0600 のファイル書き出し）や `pubkey` 取得より
        // **手前**で拒否する。REST の PUT が呼び出しの手前で行っていた「鍵が無ければ
        // 400」と同じ判定を、呼び出し口によらず必ず通る位置へ置き直したもの。
        if secret_key.trim().is_empty() {
            return Err(opencrab_actions::StartDeclined::err(
                opencrab_actions::gateway_kinds::NOSTR,
                agent_id,
                "秘密鍵（nsec）が未設定です。先に鍵を生成してください",
            ));
        }

        // 全ノート購読（author も keyword も無い）は洪水/資源浪費になるため、
        // ここ（単一チョークポイント）で拒否する（PUT enabled=false→/start バイパス封じ）。
        if config.filter_is_unbounded() {
            anyhow::bail!(
                "Nostr フィルタが空です。author か keyword を最低1つ指定してください（全ノート購読は不可）"
            );
        }

        self.stop_agent_gateway(agent_id).await;

        // nsec を含む config を 0600 で書き出す。
        NostaroCli::materialize_config(agent_id, secret_key, &config.effective_relays(), None)?;

        // 自分の pubkey は自己返信ループ防止に必須。取得できなければ **起動しない**
        // （fail-closed: 自己フィルタ無しで走ると keyword フィルタ時に自分の返信を
        // 拾って無限ループ＋LLM 支出になる）。
        let self_pubkey = self
            .cli
            .pubkey(agent_id)
            .await
            .map(|pk| pk.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "自分の pubkey を取得できませんでした（`nostaro pubkey` 必須）。\
                     自己返信ループ防止のため起動を中止します"
                )
            })?;

        let runner = self.runner.clone();
        let cli = self.cli.clone();
        let agent = agent_id.to_string();
        // self_pubkey は共有セル。identity 切替（本鍵採用）時に新 pubkey へ更新できる
        // ようにする（watch は鍵非依存なのでプロセス再起動不要。セル更新だけで自己
        // スキップが新 identity に追従する）。
        let self_pubkey_cell = Arc::new(RwLock::new(self_pubkey));
        // identity 切替の実体（runner+cli+セルを capture）。ツールから呼ばれる。
        let admin: Arc<dyn NostrIdentityAdmin> = Arc::new(LoopIdentityAdmin {
            runner: runner.clone(),
            cli: cli.clone(),
            self_pubkey: self_pubkey_cell.clone(),
        });
        let runtime = self.runtime.clone();
        let handle = tokio::spawn(async move {
            run_nostr_loop(runner, cli, agent, config, self_pubkey_cell, admin, runtime).await;
        });

        self.gateways
            .write()
            .unwrap()
            .insert(agent_id.to_string(), handle);
        info!(agent_id, "Per-agent Nostr gateway started");
        Ok(())
    }

    pub async fn stop_agent_gateway(&self, agent_id: &str) {
        let handle = self.gateways.write().unwrap().remove(agent_id);
        if let Some(handle) = handle {
            // abort でループ frame を drop → 子 nostaro は kill_on_drop で kill される。
            handle.abort();
            info!(agent_id, "Per-agent Nostr gateway stopped");
        }
    }

    pub fn is_running(&self, agent_id: &str) -> bool {
        self.gateways
            .read()
            .unwrap()
            .get(agent_id)
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }

    /// enabled な設定を DB から復元して起動する。
    pub async fn restore_from_db(&self) {
        for cfg in self.runner.list_enabled_nostr_configs() {
            let config = crate::config_from_row(&cfg);
            if let Err(e) = self
                .start_agent_gateway(&cfg.agent_id, &cfg.secret_key, config)
                .await
            {
                error!(agent_id = %cfg.agent_id, error = %e, "Failed to restore Nostr gateway");
            }
        }
    }

    pub async fn shutdown_all(&self) {
        let handles: Vec<(String, JoinHandle<()>)> =
            self.gateways.write().unwrap().drain().collect();
        for (agent_id, handle) in handles {
            handle.abort();
            info!(agent_id, "Nostr gateway stopped (shutdown_all)");
        }
    }
}

/// エージェント単位ライフサイクルの共通契約（#191 段階2）。
///
/// 既存の具象メソッドへ委譲するだけで、挙動は変えない。契約側の `start` は資格情報を
/// 引数に取らない（transport ごとに形が違う）ので、ここで DB の設定行を読んで
/// [`crate::config_from_row`] で `NostrConfig` に組み直す。
///
/// **フィルタの無制限チェックはここでは行わない。** `start_agent_gateway` の中の
/// `filter_is_unbounded` が単一チョークポイントとして担う（トレイト経由でも生の
/// 呼び出しでも同じ 1 箇所を通る）。秘密鍵の空検査も同じ理由で同じ場所にある。
///
/// ## `enabled` を見ない理由（#191 段階2 PR3）
///
/// Discord 側のガードは有効フラグも見るが、**Nostr は見てはいけない**。Nostr の
/// ハンドラは「起動が成功してから `enabled=true` にする」順序を仕様にしており
/// （失敗時に『enabled だが未稼働』の不整合を残さないため）、`PUT /nostr` は
/// **わざと `enabled=false` で行を書いてから** `start` を呼ぶ。ここで DB の
/// `enabled` を見ると、その正しい経路が毎回自分のガードに弾かれる。
///
/// 書き込み順序の方針はハンドラ側に残し、契約側のガードは**資格情報と購読条件**
/// （鍵の有無 / フィルタの有界性）に閉じる。これが「移設前と同じ判定」になる。
#[async_trait::async_trait]
impl<R: NostrAgentRunner> opencrab_actions::AgentGatewayLifecycle for NostrGatewayManager<R> {
    fn kind(&self) -> &'static str {
        opencrab_actions::gateway_kinds::NOSTR
    }

    async fn start(&self, agent_id: &str) -> anyhow::Result<()> {
        let row = self
            .runner
            .get_nostr_config(agent_id)
            .ok_or_else(|| anyhow::anyhow!("Nostr 設定がありません（agent_id={agent_id}）"))?;
        let config = crate::config_from_row(&row);
        self.start_agent_gateway(agent_id, &row.secret_key, config)
            .await
    }

    async fn stop(&self, agent_id: &str) {
        self.stop_agent_gateway(agent_id).await;
    }

    fn is_running(&self, agent_id: &str) -> bool {
        NostrGatewayManager::is_running(self, agent_id)
    }

    async fn restore_all(&self) {
        self.restore_from_db().await;
    }

    async fn shutdown_all(&self) {
        NostrGatewayManager::shutdown_all(self).await;
    }

    /// 鍵の払い出し（capability / #191 段階2 PR4）。
    ///
    /// マネージャの [`NostaroCli`] を clone して渡すので `binary_path` / timeout / vanity
    /// ゲートをそのまま継承する（HTTP ルートも LLM ツールも同じ 1 本のゲートを通る）。
    /// **ゲートウェイの稼働は要らない**（`nostaro vanity` は config を読まない）ため、
    /// `is_running` に関わらず常に `Some` を返す。
    fn key_provisioning(&self) -> Option<Arc<dyn opencrab_actions::GatewayKeyProvisioning>> {
        Some(Arc::new(crate::NostrKeyProvisioning::new(self.cli.clone())))
    }

    /// 稼働中の per-agent gateway 向けのツール実行の実体を組む（capability / #246 段階3 PR-B）。
    ///
    /// Discord の `gateway_actions_for` と対称。**稼働していなければ `None`**
    /// （`is_running` ゲート）: config.toml の materialize は stop で消えるため、稼働して
    /// いない agent へ `nostaro post` を投げても失敗する。稼働中のときだけ agent_id を焼いた
    /// `NostrGatewayActions` を返し、その `text_delivery()` が自発投稿（kind:1 broadcast）の
    /// 配送口を提供する（登録簿 `state.gateways` 経由で「テキストを配れる gateway」として
    /// 見える）。
    ///
    /// admin（identity 切替）は**付けない**: それは watch ループが持つ per-connection の状態
    /// （`start_agent_gateway` が作る self_pubkey セル）に紐づいており、ループの外から組み直す
    /// と別の状態を指してしまう。Discord が owner / A2UI 描画面を付けないのと同じ理由。
    /// 自発発話（text_delivery）には admin は要らない。
    fn gateway_actions_for(
        &self,
        agent_id: &str,
    ) -> Option<Arc<dyn opencrab_gateway::GatewayActions>> {
        if !self.is_running(agent_id) {
            return None;
        }
        Some(Arc::new(
            crate::NostrGatewayActions::new(self.cli.clone()).with_agent_id(agent_id),
        ))
    }
}

/// 処理済み event.id の bounded FIFO セット（watch 再購読時の再処理 = 二重返信を防ぐ）。
struct SeenEvents {
    order: std::collections::VecDeque<String>,
    set: std::collections::HashSet<String>,
    cap: usize,
}

impl SeenEvents {
    fn new(cap: usize) -> Self {
        Self {
            order: std::collections::VecDeque::new(),
            set: std::collections::HashSet::new(),
            cap,
        }
    }

    /// 新規なら true を返して記録。既知なら false。
    fn check_and_insert(&mut self, id: &str) -> bool {
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
type ResponseJob = Pin<Box<dyn Future<Output = ()> + Send>>;

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
struct SessionQueues {
    capacity: usize,
    /// std Mutex: ガードの下では `try_send` / `try_recv` / map 操作しかせず await を跨がない。
    queues: Mutex<HashMap<String, mpsc::Sender<ResponseJob>>>,
    /// キュー溢れで捨てた件数（観測用）。
    dropped: AtomicU64,
}

impl SessionQueues {
    fn new(capacity: usize) -> Self {
        debug_assert!(capacity >= 1, "session queue capacity must be >= 1");
        Self {
            capacity: capacity.max(1),
            queues: Mutex::new(HashMap::new()),
            dropped: AtomicU64::new(0),
        }
    }

    /// 走行中の session キュー数（回収の検証用）。
    #[cfg(test)]
    fn active_sessions(&self) -> usize {
        self.queues.lock().unwrap().len()
    }

    /// キュー溢れで捨てた累計件数（本番はログで観測する）。
    #[cfg(test)]
    fn dropped(&self) -> u64 {
        self.dropped.load(AtomicOrdering::SeqCst)
    }

    /// session のキューへ job を投入する。**ブロックしない**（`try_send` のみ）。
    fn enqueue(
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
            // 流量制限は **ここ**（ループ外）で取る。受信ループ側で取ると、session ロック
            // 待ちで何もしていないタスクが permit を占有してループ全体が止まる。
            let Ok(_permit) = permits.clone().acquire_owned().await else {
                self.queues.lock().unwrap().remove(&session_id);
                warn!(
                    agent_id,
                    session_id, "nostr: 応答 semaphore が閉じたので session consumer を終了する"
                );
                return;
            };
            job.await;
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

/// watch ループ本体。watch が落ちてもバックオフして再購読する（abort されるまで）。
/// dedup セットはループ寿命で保持する（再購読時の replay を跨いで効かせる）。
#[allow(clippy::too_many_arguments)]
async fn run_nostr_loop<R: NostrAgentRunner>(
    runner: R,
    cli: NostaroCli,
    agent_id: String,
    config: NostrConfig,
    self_pubkey: SelfPubkey,
    admin: Arc<dyn NostrIdentityAdmin>,
    runtime: Arc<NostrSessionRuntime>,
) {
    let mut seen = SeenEvents::new(512);
    // 応答生成の流量制限。watch 再購読を跨いで同じ permit プールを使う。
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_RESPONSES));
    // per-session の FIFO キュー。再購読を跨いで同じものを使う（購読が張り直されても
    // 処理待ちの順序と consumer を落とさない）。
    let queues = Arc::new(SessionQueues::new(SESSION_QUEUE_CAPACITY));
    loop {
        match run_watch_once(
            &runner,
            &cli,
            &agent_id,
            &config,
            &self_pubkey,
            &admin,
            &runtime,
            &permits,
            &queues,
            &mut seen,
        )
        .await
        {
            Ok(()) => warn!(agent_id, "nostr watch exited; restarting after backoff"),
            Err(e) => error!(agent_id, error = %e, "nostr watch error; restarting after backoff"),
        }
        tokio::time::sleep(WATCH_RESTART_DELAY).await;
    }
}

/// 1 回分の watch 購読（プロセス寿命ぶん）。
#[allow(clippy::too_many_arguments)]
async fn run_watch_once<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    config: &NostrConfig,
    self_pubkey: &SelfPubkey,
    admin: &Arc<dyn NostrIdentityAdmin>,
    runtime: &Arc<NostrSessionRuntime>,
    permits: &Arc<Semaphore>,
    queues: &Arc<SessionQueues>,
    seen: &mut SeenEvents,
) -> anyhow::Result<()> {
    let mut cmd = cli.build_watch_command(agent_id, config)?;
    let mut child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!("failed to spawn `nostaro watch` (is nostaro installed?): {e}")
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("nostaro watch produced no stdout handle"))?;
    let mut lines = BufReader::new(stdout).lines();

    info!(agent_id, relays = ?config.effective_relays(), "nostr watch subscribed");
    while let Some(line) = lines.next_line().await? {
        let Some(event) = parse_watch_line(&line) else {
            continue;
        };
        // 自分の投稿はスキップ（自己返信ループ防止）。identity 切替に追従するため
        // 共有セルから毎回読む。
        if *self_pubkey.read().unwrap() == event.pubkey {
            debug!(agent_id, "nostr: skipping own event");
            continue;
        }
        // 再処理防止（replay/重複）。
        if !seen.check_and_insert(&event.id) {
            debug!(agent_id, "nostr: skipping already-processed event");
            continue;
        }
        // 同期呼び出し（await 無し）。応答生成は session キューの consumer が引き取る。
        handle_event(
            runner, cli, agent_id, admin, runtime, permits, queues, event,
        )
        .await;
    }
    // stdout EOF → プロセス終了を回収。
    let _ = child.wait().await;
    Ok(())
}

/// 受信イベント1件を処理する（セッション記録 → 応答生成を session キューへ投入）。
///
/// **この関数は await しない**（#178）。以前は `respond_serialized(...).await` を受信
/// ループ内で直接 await していたため、per-session ロックを resume が握っている間
/// **ループ全体（全セッション・全相手）が停止**し、`nostaro watch` の stdout も読まれず
/// 滞留した。S3a で resume が日常化したため常態化していた。
///
/// その後 `tokio::spawn` へ出したが、それだけでは **同一セッションの連投の処理順が
/// 「どの spawn タスクが先にロックを取るか」で決まる**（順序保証が壊れる）うえ、
/// permit をループ内で取っていたため上限が埋まると再び全相手の受信が止まった。
/// 現在は [`SessionQueues`] へ投入するだけで、FIFO 処理・直列化・流量制限はすべて
/// consumer タスク側（ループ外）が担う。
///
/// 直列化（#168）はそのまま成立する: ロック取得は [`NostrResponder::respond_serialized`]
/// に閉じているので、consumer が回す job でも同一セッションの inbound / resume が直列化
/// される。セッションの用意と受信の転記は**ループ内で同期的に**済ませる: DB 書き込み
/// のみで await しないうえ、job 側へ回すと連投で転記順が入れ替わる。
#[allow(clippy::too_many_arguments)]
async fn handle_event<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    admin: &Arc<dyn NostrIdentityAdmin>,
    runtime: &Arc<NostrSessionRuntime>,
    permits: &Arc<Semaphore>,
    queues: &Arc<SessionQueues>,
    event: NostrEvent,
) {
    // author 単位のセッション（1 相手 = 1 会話）。
    let session_id = nostr_session_id(agent_id, &event.pubkey);

    runner.ensure_session(&session_id, &[agent_id.to_string()], "Nostr", "{}", "nostr");
    runner.record_inbound_message(
        opencrab_actions::TranscriptSource::Nostr,
        &opencrab_actions::InboundMessageRecord {
            session_id: &session_id,
            sender_id: &event.pubkey,
            sender_name: &event.author_label(),
            avatar_url: None,
            channel_id: None,
            pubkey: Some(&event.pubkey),
            text: &event.content,
            image_urls: &[],
        },
    );

    // クロスゲートウェイ転記（issue #252 段階 A）: 自分宛の受信を、エージェント単位で
    // 設定した Discord チャンネル（webhook）へ転記する。設定が有効なときだけ配送する
    // （未設定 / 無効 → `resolve_nostr_relay_target` が None を返し、1 件も飛ばない = fail-closed）。
    //
    // ここは受信ループ内（#178: await しない）。宛先の解決は同期 DB 読み 1 回、実際の送信は
    // 実装側で非ブロック（fire-and-forget）。送信失敗は実装側でログのみに留め、応答生成や
    // 他セッションの受信を巻き込まない。Nostr 側は Discord を型で名指しせず、actions 層の
    // 共通口（`WebhookConfig`）を通す。
    if let Some(target) = runner.resolve_nostr_relay_target(agent_id) {
        let relay_text = format!(
            "[Nostr / {kind}] {author}\n{body}",
            kind = event.inbound_kind_label(),
            author = event.author_label(),
            body = event.content,
        );
        runner.relay_inbound_notification(&target, relay_text);
    }

    let prompt_suffix = format!(
        "[Nostr] {author} さんの投稿への応答です。返信するなら \
         nostr_reply(target=\"{target}\") を使ってください（target は返信先ノート）。\
         返信不要なら NO_REPLY とだけ答えてください。",
        author = event.author_label(),
        target = event.reply_target(),
    );

    let responder = NostrResponder::new(
        runner.clone(),
        cli.clone(),
        runtime.clone(),
        admin.clone(),
        agent_id,
    );
    let reply_target = event.reply_target().to_string();
    let event_id = event.id.clone();
    let job_session_id = session_id.clone();
    // 流量制限（permit）は **consumer タスクの内側**で取る（`run_consumer` 参照）。
    // ここ（受信ループ内）で取ると、session ロック待ちで何もしていないタスクが permit を
    // 占有し、受信ループ全体＝そのエージェントの全相手の受信が止まる。
    let job: ResponseJob = Box::pin(async move {
        responder
            .respond_serialized(
                &job_session_id,
                &reply_target,
                &prompt_suffix,
                Some(&event_id),
            )
            .await;
    });
    queues.enqueue(agent_id, &session_id, permits, job);
}

/// watch ループが握る identity 切替の実体。runner（DB）+ cli + self_pubkey セルを capture し、
/// 生成鍵を本鍵に採用する。**watch プロセスは再起動しない**（鍵非依存）。self_pubkey セルを
/// 新 pubkey へ更新するだけで、以後の自己スキップが新 identity に追従する。
struct LoopIdentityAdmin<R: NostrAgentRunner> {
    runner: R,
    cli: NostaroCli,
    self_pubkey: SelfPubkey,
}

#[async_trait::async_trait]
impl<R: NostrAgentRunner> NostrIdentityAdmin for LoopIdentityAdmin<R> {
    async fn adopt_generated_identity(&self, agent_id: &str, npub: &str) -> anyhow::Result<String> {
        // 生成鍵（自分が作ったもの）の nsec をサーバ内から読む。存在チェックで
        // 「自分が生成した鍵のみ採用可」を担保。秘密鍵は外へ出さない。
        let nsec = NostaroCli::read_generated_key(agent_id, npub)?;
        // 既存設定（relays/filter を継承）。未設定なら採用しない。
        let row = self.runner.get_nostr_config(agent_id).ok_or_else(|| {
            anyhow::anyhow!("Nostr 未設定です。先に Nostr を設定してから本鍵を切り替えてください")
        })?;
        let config = crate::config_from_row(&row);
        let relays = config.effective_relays();
        // ロールバック用に旧状態を控える。
        let old_secret = row.secret_key.clone();
        let old_pubkey = self.self_pubkey.read().unwrap().clone();

        // 1) config.toml を新鍵で再生成（0600・アトミック）。send/pubkey はこれを読む。
        NostaroCli::materialize_config(agent_id, &nsec, &relays, None)?;

        // 2) 新 pubkey を取得。**fail-closed**: 取れないと自己スキップが旧 pubkey のまま
        //    になり、新 identity の自分の投稿を拾って自己返信無限ループ＋LLM 課金になる
        //    （起動時ガードと同じ危険）。config を旧鍵へ巻き戻して中止する。
        let new_pubkey = match self.cli.pubkey(agent_id).await {
            Ok(pk) if !pk.trim().is_empty() => pk.trim().to_string(),
            _ => {
                if let Err(re) =
                    NostaroCli::materialize_config(agent_id, &old_secret, &relays, None)
                {
                    error!(agent_id, error = %re, "nostr: identity 切替のロールバック（config復元）に失敗");
                }
                anyhow::bail!(
                    "新しい鍵の pubkey を取得できませんでした。自己返信ループ防止のため切替を中止しました（設定は元に戻しました）"
                );
            }
        };

        // 3) 自己スキップ用セルを更新（以後の自己スキップが新 identity 追従）。
        *self.self_pubkey.write().unwrap() = new_pubkey;

        // 4) DB を最後に更新。失敗したら config/セルを旧状態へ巻き戻す（DB=旧 / config=新
        //    の不整合＝再起動で勝手に切替完了する事故を防ぐ）。
        if let Err(e) = self.runner.set_nostr_secret_key(agent_id, &nsec) {
            if let Err(re) = NostaroCli::materialize_config(agent_id, &old_secret, &relays, None) {
                error!(agent_id, error = %re, "nostr: identity 切替のロールバック（config復元）に失敗");
            }
            *self.self_pubkey.write().unwrap() = old_pubkey;
            return Err(e).context("DB の本鍵更新に失敗（設定を元に戻しました）");
        }
        Ok(npub.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Mutex;

    use opencrab_actions::webhook_target::WebhookConfig;
    use opencrab_actions::RunRequest;
    use opencrab_core::EngineResult;
    use opencrab_db::queries::AgentNostrConfigRow;

    /// 受信ループの非ブロック性・順序保証の検証用の最小 runner。LLM も DB も使わない。
    #[derive(Clone)]
    struct SlowRunner {
        delay: Duration,
        inflight: Arc<AtomicUsize>,
        max_inflight: Arc<AtomicUsize>,
        /// 転記された受信メッセージ（順序の検証用）。
        recorded: Arc<Mutex<Vec<String>>>,
        /// 応答生成を**開始**した順（reply_target）。
        started: Arc<Mutex<Vec<String>>>,
        /// 応答生成を**完了**した順（reply_target = 実際に返信が飛ぶ順）。
        finished: Arc<Mutex<Vec<String>>>,
        /// 転記先の解決結果（#252）。`None` = 未設定（転記しない）。
        relay_target: Option<WebhookConfig>,
        /// 実際に転記口へ渡った本文（配送口のスパイ / #252）。
        relayed: Arc<Mutex<Vec<String>>>,
    }

    impl SlowRunner {
        fn new(delay: Duration) -> Self {
            Self {
                delay,
                inflight: Arc::new(AtomicUsize::new(0)),
                max_inflight: Arc::new(AtomicUsize::new(0)),
                recorded: Arc::new(Mutex::new(Vec::new())),
                started: Arc::new(Mutex::new(Vec::new())),
                finished: Arc::new(Mutex::new(Vec::new())),
                relay_target: None,
                relayed: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// 転記先を有効化した runner（#252 のフック検証用）。
        fn with_relay_target(mut self, url: &str) -> Self {
            self.relay_target = Some(WebhookConfig {
                url: url.to_string(),
                events: None,
            });
            self
        }

        fn finished_len(&self) -> usize {
            self.finished.lock().unwrap().len()
        }

        fn snapshot(list: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
            list.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl opencrab_actions::AgentRuntime for SlowRunner {
        async fn run_agent_response(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
            let target = req.reply_target.clone().unwrap_or_default();
            self.started.lock().unwrap().push(target.clone());
            let now = self.inflight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            self.max_inflight.fetch_max(now, AtomicOrdering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.inflight.fetch_sub(1, AtomicOrdering::SeqCst);
            self.finished.lock().unwrap().push(target);
            // NO_REPLY にして nostaro（外部プロセス）を一切呼ばない。
            Ok(EngineResult {
                response: "NO_REPLY".to_string(),
                iterations: 1,
                tool_calls_made: 0,
                stopped_by_limit: false,
                xml_fallback_parses: 0,
            })
        }

        fn build_agent_context(&self, _agent_id: &str) -> (String, String) {
            ("base".to_string(), "テストくん".to_string())
        }

        fn build_conversation_string(
            &self,
            _session_id: &str,
            _agent_id: &str,
            _budget: usize,
        ) -> anyhow::Result<String> {
            Ok("conversation".to_string())
        }

        fn context_budget_tokens(&self, _agent_id: &str) -> usize {
            1000
        }

        fn has_llm_providers(&self) -> bool {
            true
        }

        fn ensure_session(&self, _s: &str, _a: &[String], _t: &str, _m: &str, _mode: &str) {}

        fn record_inbound_message(
            &self,
            source: opencrab_actions::TranscriptSource,
            record: &opencrab_actions::InboundMessageRecord<'_>,
        ) {
            assert_eq!(source, opencrab_actions::TranscriptSource::Nostr);
            self.recorded.lock().unwrap().push(record.text.to_string());
        }

        fn on_inbound_message(
            &self,
            _source: opencrab_actions::TranscriptSource,
            _agent_id: &str,
            _record: &opencrab_actions::InboundMessageRecord<'_>,
        ) {
            // 受信フック（#156 S4）。Nostr の受信はまだ配線していないので no-op。
        }

        // 以下はこの経路が使わない（NO_REPLY で返すので応答転記も走らない）。
        fn record_outbound_reply(
            &self,
            _source: opencrab_actions::TranscriptSource,
            _record: &opencrab_actions::OutboundReplyRecord<'_>,
        ) {
            unimplemented!("nostr の fake は NO_REPLY を返すので応答転記を使わない")
        }

        fn record_interaction_response(
            &self,
            _agent_id: &str,
            _session_id: &str,
            _record: &opencrab_actions::InteractionRecord<'_>,
        ) {
            unimplemented!("nostr の fake は A2UI interaction を使わない")
        }

        fn record_agent_no_reply(&self, _agent_id: &str, _session_id: &str) {
            unimplemented!("nostr の fake は NO_REPLY 記録を使わない")
        }

        fn session_theme(&self, _session_id: &str) -> Option<String> {
            unimplemented!("nostr の fake は session_theme を使わない")
        }

        fn mark_interaction_status(&self, _i: &str, _s: &str, _r: Option<&str>, _u: Option<&str>) {
            unimplemented!("nostr の fake は A2UI interaction を使わない")
        }

        fn cleanup_stale_interactions(&self) {
            unimplemented!("nostr の fake は A2UI interaction を使わない")
        }

        fn cleanup_stale_interactions_for_agent(&self, _agent_id: &str) {
            unimplemented!("nostr の fake は A2UI interaction を使わない")
        }
    }

    impl NostrAgentRunner for SlowRunner {
        fn list_enabled_nostr_configs(&self) -> Vec<AgentNostrConfigRow> {
            Vec::new()
        }

        fn get_nostr_config(&self, _agent_id: &str) -> Option<AgentNostrConfigRow> {
            None
        }

        fn set_nostr_secret_key(&self, _a: &str, _s: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn resolve_nostr_relay_target(&self, _agent_id: &str) -> Option<WebhookConfig> {
            self.relay_target.clone()
        }

        fn relay_inbound_notification(&self, _target: &WebhookConfig, text: String) {
            // 配送口のスパイ: 実際に転記へ回った本文を記録する（HTTP は出さない）。
            self.relayed.lock().unwrap().push(text);
        }
    }

    struct NoopAdmin;

    #[async_trait::async_trait]
    impl NostrIdentityAdmin for NoopAdmin {
        async fn adopt_generated_identity(
            &self,
            _agent_id: &str,
            npub: &str,
        ) -> anyhow::Result<String> {
            Ok(npub.to_string())
        }
    }

    fn event(id: &str, pubkey: &str, content: &str) -> NostrEvent {
        NostrEvent {
            id: id.to_string(),
            pubkey: pubkey.to_string(),
            npub: None,
            note_id: Some(format!("note1{id}")),
            author_name: None,
            created_at: 0,
            kind: 1,
            content: content.to_string(),
            tags: Vec::new(),
        }
    }

    /// 受信ループ相当の呼び出しを組み立てるテスト用ハーネス。
    struct Harness {
        runner: SlowRunner,
        admin: Arc<dyn NostrIdentityAdmin>,
        runtime: Arc<NostrSessionRuntime>,
        permits: Arc<Semaphore>,
        queues: Arc<SessionQueues>,
        cli: NostaroCli,
        agent_id: String,
    }

    impl Harness {
        fn new(agent_id: &str, delay: Duration, permits: usize, capacity: usize) -> Self {
            Self::with_runner(agent_id, SlowRunner::new(delay), permits, capacity)
        }

        fn with_runner(
            agent_id: &str,
            runner: SlowRunner,
            permits: usize,
            capacity: usize,
        ) -> Self {
            Self {
                runner,
                admin: Arc::new(NoopAdmin),
                runtime: Arc::new(NostrSessionRuntime::new()),
                permits: Arc::new(Semaphore::new(permits)),
                queues: Arc::new(SessionQueues::new(capacity)),
                cli: NostaroCli::new(),
                agent_id: agent_id.to_string(),
            }
        }

        /// watch ループが 1 行読んだのと同じ処理（同期・await 無し）。
        async fn feed(&self, id: &str, pubkey: &str, content: &str) {
            handle_event(
                &self.runner,
                &self.cli,
                &self.agent_id,
                &self.admin,
                &self.runtime,
                &self.permits,
                &self.queues,
                event(id, pubkey, content),
            )
            .await;
        }

        /// 応答生成が `n` 件完了するまで待つ（タイムアウトしたら false）。
        async fn wait_finished(&self, n: usize, timeout: Duration) -> bool {
            let deadline = std::time::Instant::now() + timeout;
            while std::time::Instant::now() < deadline {
                if self.runner.finished_len() >= n {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            self.runner.finished_len() >= n
        }
    }

    /// [P1 回帰 / #168] 同一セッション（同一相手）の連投は**投入順どおり**に処理される。
    ///
    /// 応答生成を素朴に `tokio::spawn` へ出していたときは「どの spawn タスクが先に
    /// session ロックを取るか」で順序が決まり、5 通目への返信が 1 通目より先に飛ぶ
    /// ことがあった（各返信は勝ったタスクの `reply_target` に紐づく）。
    /// multi_thread ランタイムで複数回試行して安定することを見る。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_session_events_are_processed_in_submission_order() {
        const IDS: [&str; 8] = [
            "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth",
        ];
        let expected: Vec<String> = IDS.iter().map(|i| format!("note1{i}")).collect();

        // spawn 順の運任せを暴くには複数回試行が必要（1 回だと偶然通ることがある）。
        for trial in 0..5 {
            let h = Harness::new("agent-order", Duration::from_millis(5), 8, 32);
            for id in IDS {
                h.feed(id, "pk-chatty", id).await;
            }
            assert!(
                h.wait_finished(IDS.len(), Duration::from_secs(5)).await,
                "試行{trial}: 応答生成が完了しない"
            );

            // 転記順・開始順・完了順（= 返信が飛ぶ順）がすべて投入順と一致する。
            assert_eq!(
                SlowRunner::snapshot(&h.runner.recorded),
                IDS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "試行{trial}: 会話への転記順が投入順と違う"
            );
            assert_eq!(
                SlowRunner::snapshot(&h.runner.started),
                expected,
                "試行{trial}: 同一セッションの処理順が投入順と違う"
            );
            assert_eq!(
                SlowRunner::snapshot(&h.runner.finished),
                expected,
                "試行{trial}: 返信順が投入順と違う"
            );
            // 同一セッションは直列（二重投稿しない）。
            assert_eq!(
                h.runner.max_inflight.load(AtomicOrdering::SeqCst),
                1,
                "試行{trial}: 同一セッションの応答生成が並行した"
            );
        }
    }

    /// [P1 回帰 / #178] 受信ループは応答生成を await しない。
    ///
    /// 以前は `respond_serialized(...).await` をループ内で直接呼んでいたため、長い応答の
    /// あいだ**全セッション・全相手**の受信が止まった（`nostaro watch` の stdout も
    /// 読まれず滞留）。ここでは 2 件の `handle_event` が即座に返ること、かつ別セッション
    /// の応答が並行することを見る。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn handle_event_does_not_block_the_receive_loop() {
        let h = Harness::new("agent-loop", Duration::from_millis(300), 8, 32);

        let started = std::time::Instant::now();
        // 別々の相手（別セッション）から 2 件。ループ相当の直列呼び出し。
        h.feed("e1", "pk-a", "1件目").await;
        h.feed("e2", "pk-b", "2件目").await;
        let elapsed = started.elapsed();

        // ループは応答生成（300ms）を待たずに次へ進んでいる。
        assert!(
            elapsed < Duration::from_millis(150),
            "受信ループが応答生成でブロックしている: {elapsed:?}"
        );
        // 受信の転記はループ内で同期的に済んでいる（順序も保たれる）。
        assert_eq!(
            SlowRunner::snapshot(&h.runner.recorded),
            vec!["1件目".to_string(), "2件目".to_string()]
        );

        // 別セッションの応答生成は並行して走る。
        for _ in 0..100 {
            if h.runner.max_inflight.load(AtomicOrdering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            h.runner.max_inflight.load(AtomicOrdering::SeqCst) >= 2,
            "別セッションの応答生成が並行していない（head-of-line blocking）"
        );
    }

    /// [P1 回帰 / #178] permit 待ちが**受信を止めない**（head-of-line blocking なし）。
    ///
    /// permit を受信ループ内で取っていたときは、ロック待ちで何もしていないタスクが permit
    /// を占有し、上限が埋まった時点でループ全体（＝全相手の受信）が停止した。レビュアーの
    /// 実験と同型: permits=2 / 同一セッション 2 件 → 別セッション 1 件。別セッションの
    /// 応答生成が、詰まっているセッションの完了を待たずに始まることを見る。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn permit_starvation_does_not_stall_the_receive_loop() {
        for trial in 0..3 {
            let h = Harness::new("agent-starve", Duration::from_millis(300), 2, 32);

            // 多弁な相手（同一セッション）が permit を使い切ろうとする。
            h.feed("s1", "pk-chatty", "1").await;
            h.feed("s2", "pk-chatty", "2").await;
            // 別の相手。ここでループが止まってはいけない。
            let started = std::time::Instant::now();
            h.feed("s3", "pk-quiet", "3").await;
            let loop_stall = started.elapsed();

            assert!(
                loop_stall < Duration::from_millis(50),
                "試行{trial}: 別セッションの handle_event がループを止めた: {loop_stall:?}"
            );

            // 別セッションの応答生成は、詰まっているセッション（300ms×2）を待たずに始まる。
            let mut quiet_started = false;
            let deadline = std::time::Instant::now() + Duration::from_millis(200);
            while std::time::Instant::now() < deadline {
                if SlowRunner::snapshot(&h.runner.started)
                    .iter()
                    .any(|t| t == "note1s3")
                {
                    quiet_started = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(
                quiet_started,
                "試行{trial}: 別セッションの応答生成が同一セッションの完了を待たされた"
            );
            // 同時に 2 本走った = permit がロック待ちに浪費されていない。
            assert!(
                h.runner.max_inflight.load(AtomicOrdering::SeqCst) >= 2,
                "試行{trial}: permit がロック待ちのタスクに占有されている"
            );
        }
    }

    /// [#178] permit をタスク内側で取っても同時実行上限は守られる。
    ///
    /// permit=1 なら別セッション 3 件でも応答生成は 1 本ずつ（`max_inflight == 1`）。
    /// ループ自体はブロックしない（上限は実同時実行だけを絞る）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_responses_are_capped_by_permits() {
        let h = Harness::new("agent-cap", Duration::from_millis(80), 1, 32);

        let started = std::time::Instant::now();
        h.feed("c1", "pk-a", "1").await;
        h.feed("c2", "pk-b", "2").await;
        h.feed("c3", "pk-c", "3").await;
        let loop_stall = started.elapsed();
        assert!(
            loop_stall < Duration::from_millis(50),
            "上限が受信ループを止めている: {loop_stall:?}"
        );

        assert!(
            h.wait_finished(3, Duration::from_secs(5)).await,
            "応答生成が完了しない"
        );
        assert_eq!(
            h.runner.max_inflight.load(AtomicOrdering::SeqCst),
            1,
            "permit=1 のとき応答生成は 1 本ずつ"
        );
        // 直列化されたぶんの時間はかかっている（上限が実在する証拠）。
        assert!(
            started.elapsed() >= Duration::from_millis(240),
            "同時実行上限（permit）が効いていない: {:?}",
            started.elapsed()
        );
    }

    /// [#168] session キューが溢れたぶんは**ログに残して**捨てる（黙って捨てない）。
    ///
    /// permit を 0 本にして consumer を確実に止め、capacity を超える連投を流し込む。
    /// 受け付けられるのは「consumer が取り出した 1 件 + バッファ capacity 件」まで。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn session_queue_overflow_is_dropped_and_counted() {
        const CAPACITY: usize = 2;
        const FLOOD: usize = 20;
        // permits=0 → consumer は permit 待ちで止まり、キューが確実に埋まる。
        let h = Harness::new("agent-flood", Duration::from_millis(1), 0, CAPACITY);

        for i in 0..FLOOD {
            h.feed(&format!("f{i}"), "pk-flood", &format!("{i}")).await;
        }

        // 受理されうる上限は inflight 1 + バッファ CAPACITY。
        let accepted = FLOOD as u64 - h.queues.dropped();
        assert!(
            accepted <= (1 + CAPACITY) as u64,
            "キュー上限を超えて受理された: accepted={accepted}"
        );
        assert!(
            h.queues.dropped() >= (FLOOD - 1 - CAPACITY) as u64,
            "溢れが捨てられていない: dropped={}",
            h.queues.dropped()
        );
        // 捨てても投稿本文は会話履歴に転記済み（次の応答の文脈に載る）。
        assert_eq!(SlowRunner::snapshot(&h.runner.recorded).len(), FLOOD);
    }

    /// [#168] アイドルになった session の consumer タスク / チャネルは回収される。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_session_queue_is_reclaimed() {
        let h = Harness::new("agent-reclaim", Duration::from_millis(5), 8, 32);
        h.feed("r1", "pk-a", "1").await;
        h.feed("r2", "pk-b", "2").await;
        assert!(
            h.queues.active_sessions() > 0,
            "投入直後は session キューが存在する"
        );

        assert!(
            h.wait_finished(2, Duration::from_secs(5)).await,
            "応答生成が完了しない"
        );
        // 完了後、キューは空になり consumer は自分ごと回収される。
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline && h.queues.active_sessions() > 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            h.queues.active_sessions(),
            0,
            "アイドルな session キューが回収されていない（task/チャネルのリーク）"
        );

        // 回収後に再投入しても普通に処理される（回収とレースしても取りこぼさない）。
        h.feed("r3", "pk-a", "3").await;
        assert!(
            h.wait_finished(3, Duration::from_secs(5)).await,
            "回収後の再投入が処理されない"
        );
    }

    /// [#252 段階 A] 転記が有効なら、受信 1 件につき配送口が**ちょうど 1 回**呼ばれる。
    /// 転記本文には送信者ラベル・種別・本文が載る。転記は受信ループ内で同期的に済む。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_fires_once_per_inbound_when_configured() {
        const URL: &str = "https://discord.com/api/webhooks/1/tok";
        let runner = SlowRunner::new(Duration::from_millis(1)).with_relay_target(URL);
        let h = Harness::with_runner("agent-relay", runner, 8, 32);

        h.feed("r1", "pk-a", "こんにちは").await;

        let relayed = SlowRunner::snapshot(&h.runner.relayed);
        assert_eq!(relayed.len(), 1, "受信 1 件につき転記は 1 回");
        assert!(
            relayed[0].contains("こんにちは"),
            "本文が載る: {}",
            relayed[0]
        );
        // author_label は name/npub 無しなので短縮 pubkey。種別見出しも載る。
        assert!(relayed[0].contains("pk-a"), "送信者が載る: {}", relayed[0]);
        assert!(
            relayed[0].contains("メンション") || relayed[0].contains("[Nostr"),
            "種別見出しが載る: {}",
            relayed[0]
        );

        // 2 件目も 1 回ずつ増える（受信ごとに 1 回）。
        h.feed("r2", "pk-a", "ふたつめ").await;
        assert_eq!(SlowRunner::snapshot(&h.runner.relayed).len(), 2);
    }

    /// [#252 段階 A / fail-closed] 転記が未設定なら、受信があっても 1 件も飛ばない。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_is_fail_closed_when_unconfigured() {
        // relay_target を設定しない runner（= resolve が None を返す）。
        let h = Harness::new("agent-norelay", Duration::from_millis(1), 8, 32);

        h.feed("n1", "pk-a", "本文").await;
        h.feed("n2", "pk-b", "本文2").await;

        assert!(
            SlowRunner::snapshot(&h.runner.relayed).is_empty(),
            "未設定なら転記は 1 件も飛ばない（fail-closed）"
        );
        // 受信自体は通常どおり転記（会話履歴）される。
        assert_eq!(SlowRunner::snapshot(&h.runner.recorded).len(), 2);
    }

    #[test]
    fn test_seen_events_dedup_and_eviction() {
        let mut seen = SeenEvents::new(2);
        assert!(seen.check_and_insert("a")); // 新規
        assert!(!seen.check_and_insert("a")); // 既知
        assert!(seen.check_and_insert("b"));
        // cap=2 を超えると最古（a）を追い出す。
        assert!(seen.check_and_insert("c"));
        // a は追い出されたので再び新規扱い（replay 耐性は cap ぶん）。
        assert!(seen.check_and_insert("a"));
        // b はまだ保持（直近）… ではなく a 追加で b が最古になり追い出される可能性。
        // 少なくとも直近の c は既知のまま。
        assert!(!seen.check_and_insert("c"));
    }

    /// [#246 段階3 PR-B] `gateway_actions_for` は **稼働中の agent にだけ** capability を返し、
    /// その `text_delivery()` が自発投稿の配送口（`Some`）を提供する。稼働していない agent は
    /// `None`（config.toml が無い＝post が失敗する状態へツールを生やさない / fail-closed）。
    #[tokio::test]
    async fn gateway_actions_for_is_gated_on_is_running_and_exposes_text_delivery() {
        use opencrab_actions::AgentGatewayLifecycle;

        let mgr = NostrGatewayManager::new(SlowRunner::new(Duration::from_millis(1)));

        // 稼働中の agent を模す: 終わらないダミータスクの handle を登録簿へ挿す
        // （`is_running` は handle の生死で判定する）。
        let handle = tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
        mgr.gateways
            .write()
            .unwrap()
            .insert("agent-live".to_string(), handle);

        // 稼働中 → Some、かつ text_delivery() が Some（テキストを配れる gateway として見える）。
        assert!(mgr.is_running("agent-live"));
        let actions = AgentGatewayLifecycle::gateway_actions_for(&mgr, "agent-live");
        let actions = actions.expect("稼働中の agent には capability を返す");
        assert!(
            actions.text_delivery().is_some(),
            "自発投稿の配送口を提供する"
        );

        // 稼働していない agent → None（None を返し、post を呼ばない）。
        assert!(!mgr.is_running("agent-idle"));
        assert!(
            AgentGatewayLifecycle::gateway_actions_for(&mgr, "agent-idle").is_none(),
            "稼働していない agent には capability を返さない（fail-closed）"
        );

        // ダミータスクを回収する。
        mgr.gateways
            .write()
            .unwrap()
            .remove("agent-live")
            .unwrap()
            .abort();
    }
}
