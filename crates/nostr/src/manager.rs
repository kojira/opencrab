//! Per-agent Nostr sub-gateway マネージャ + watch ループ。
//!
//! Discord の `DiscordGatewayManager` と同型。エージェント毎に nostaro の `watch --json`
//! を spawn し、JSONL イベントを読んで `run_agent_response` → 返信する。
//!
//! **受信ループは応答生成でブロックしない**（#178）。応答生成（会話再構築 → LLM →
//! 返信）は受信ループの外へ出し、ループは即次の行へ進む。
//!
//! ただし単純に `tokio::spawn` へ投げると、**連投の処理順が「どの spawn タスクが先に
//! session ロックを取るか」で決まる**（= ランダム）。5 通目への返信が 1 通目より先に
//! 届きうる。そこで [`SessionQueues`] を挟み、**session ごとに 1 本の consumer タスク**
//! が bounded な mpsc から FIFO で取り出して処理する（per-session 直列 + 順序保証、
//! 別セッションは並行）。consumer はキューが空になったら自分ごと回収される
//! （task/チャネルのリーク防止）。
//!
//! **#323 以降、Nostr の session は agent 単位で 1 本**（`nostr-{agent_id}`）なので、
//! このループが持つ consumer は実質 1 本になり、そのエージェントの応答生成は相手が
//! 誰であれ 1 件ずつ直列に走る（オーナー方針「発言し終わるまで次の LLM を呼ばない」）。
//! [`SessionQueues`] は「1 本前提」に作り替えていない: キュー束は session_id をキーに
//! した写像のままで、1 本になっても回収・再投入・溢れの扱いは変わらない。permit も
//! consumer の内側で取り、`await` が終われば返るのでデッドロックにも枯渇にもならない。
//!
//! 同時実行上限（[`MAX_CONCURRENT_RESPONSES`]）の permit は **consumer タスクの内側**
//! で取る。受信ループ側で取ると「session ロック待ちで何もしていないタスク」が permit を
//! 占有し、上限が埋まった時点でループ全体（＝そのエージェントの全受信）が止まる
//! （head-of-line blocking / #178 が直そうとしたバグと同型）。

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
///
/// #323 で session が agent 単位の 1 本になったため、このエージェントが実際に使う
/// permit は常に 1 枚（実効同時実行数 = 1）。**値は変えない**: 上限は「暴走したときの
/// 天井」であって目標値ではなく、1 本になったからといって天井を下げる理由も、
/// 並行を取り戻すために上げる理由も無い（直列化は意図した挙動）。
const MAX_CONCURRENT_RESPONSES: usize = 8;

/// per-session の inbound キュー容量（per-agent / #168）。
///
/// 応答生成は LLM 1 往復ぶんかかるので、連投され続けるとキューは伸びる。
/// 無制限に伸ばすとメモリと「もう誰も待っていない返信」が溜まるだけなので上限を置き、
/// 溢れたぶんは**ログに残して**捨てる（本文は転記済みなので次の応答の会話履歴に載る）。
///
/// #323 の挙動変化: session が agent 単位の 1 本になったので、この 32 件は
/// 「相手 1 人あたり」ではなく**そのエージェント宛の受信の合計**になる。**値は変えない**
/// （新しい上限を足さない / 元の上限を据え置く）。溢れても本文は転記済みで、次の応答の
/// 会話履歴には載る — 1 本化で履歴が揃うぶん、捨てられた回のぶんも文脈からは追える。
const SESSION_QUEUE_CAPACITY: usize = 32;

/// 稼働中 gateway の登録簿（agent_id → watch ループの JoinHandle）。
///
/// `Arc`: identity 採用 capability（[`NostrIdentityProvisioner`]）が同じ登録簿を見て
/// 起動・生存確認できるようにするため（マネージャ本体と capability が別インスタンスでも
/// 同一の登録簿を共有する）。
type GatewayMap = Arc<RwLock<HashMap<String, JoinHandle<()>>>>;

/// 稼働中 gateway の per-agent ホットスワップ admin（agent_id → identity 切替の実体）。
///
/// watch ループ起動時に登録し、停止時に外す。identity 採用 capability が「稼働中なら
/// この admin で in-place ホットスワップ、無ければ bootstrap 起動」を判定するのに使う。
type AdminMap = Arc<RwLock<HashMap<String, Arc<dyn NostrIdentityAdmin>>>>;

pub struct NostrGatewayManager<R: NostrAgentRunner> {
    // std RwLock: is_running を同期メソッドにするため。ガードは await を跨がない。
    gateways: GatewayMap,
    /// 稼働中の per-agent 採用 admin（#264）。`gateways` と生死を揃える。
    admins: AdminMap,
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
            gateways: Arc::new(RwLock::new(HashMap::new())),
            admins: Arc::new(RwLock::new(HashMap::new())),
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
    /// 取得（自己返信ループ防止）して watch ループを spawn する。起動の実体は共有 free fn
    /// [`spawn_agent_gateway`]（identity 採用 capability も同じ経路を通る）。
    pub async fn start_agent_gateway(
        &self,
        agent_id: &str,
        secret_key: &str,
        config: NostrConfig,
    ) -> anyhow::Result<()> {
        spawn_agent_gateway(
            &self.gateways,
            &self.admins,
            &self.runner,
            &self.cli,
            &self.runtime,
            agent_id,
            secret_key,
            config,
        )
        .await
    }

    pub async fn stop_agent_gateway(&self, agent_id: &str) {
        stop_gateway(&self.gateways, &self.admins, agent_id);
    }

    pub fn is_running(&self, agent_id: &str) -> bool {
        self.gateways
            .read()
            .unwrap()
            .get(agent_id)
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }

    /// 生成鍵の採用（identity 切替）capability を返す（#264）。
    ///
    /// `gateways` / `admins` の**同じ登録簿**（Arc）を共有する実体を返すので、採用時の
    /// bootstrap 起動・稼働中判定・ホットスワップが本体と一貫する。
    pub fn identity_provisioner(&self) -> Arc<NostrIdentityProvisioner<R>> {
        Arc::new(NostrIdentityProvisioner {
            gateways: self.gateways.clone(),
            admins: self.admins.clone(),
            runner: self.runner.clone(),
            cli: self.cli.clone(),
            runtime: self.runtime.clone(),
        })
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
        // 採用 admin も一緒に落とす（生死を gateways と揃える）。
        self.admins.write().unwrap().clear();
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
/// **起動条件の検査はここでは行わない。** 秘密鍵の空検査は `start_agent_gateway` の中が
/// 単一チョークポイントとして担う（トレイト経由でも生の呼び出しでも同じ 1 箇所を通る）。
/// 購読が「自分宛」に閉じることの担保は `NostaroCli::build_watch_command`（`--match=any` /
/// `--no-mention-only` を渡さない）に移した（#271/#278）。
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

    /// 生成鍵の採用（identity 切替）capability（#264）。
    ///
    /// server-own の `nostr_switch_identity` がここから引く。稼働の有無を必要としない
    /// （未稼働なら bootstrap 起動＝接続、稼働中ならホットスワップ）ので、`key_provisioning`
    /// と同じく `is_running` に関わらず常に `Some` を返す。
    fn identity_provisioning(
        &self,
    ) -> Option<Arc<dyn opencrab_actions::GatewayIdentityProvisioning>> {
        Some(self.identity_provisioner())
    }

    /// 薄い nostaro passthrough capability（#268）。
    ///
    /// マネージャの [`NostaroCli`] を clone して渡すので `binary_path` / timeout をそのまま
    /// 継承する。`key_provisioning` と同じく**稼働は要らない**（config.toml さえあれば投稿
    /// できる）ため `is_running` に関わらず常に `Some` を返す。deny・config 固定・未
    /// materialize の明示エラー・nsec マスクは `NostaroCli::run_passthrough` の内側。
    fn nostr_passthrough(&self) -> Option<Arc<dyn opencrab_actions::GatewayNostrPassthrough>> {
        Some(Arc::new(crate::NostrPassthrough::new(self.cli.clone())))
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
    // #514: DM（kind:4 NIP-04 / kind:1059 NIP-17 gift wrap）は**一切扱わない**。
    // 会話へ入れず・応答せず・記録もせず、公開リプライへフォールバックもしない。
    // 暗号化 DM は「今は安全」でも秘密鍵が漏れた時点で過去に遡って全部読めるため、
    // 「暗号化されているから private を書いてよい」という誤った安心を前提ごと無くす
    // （オーナー決定）。この drop は購読除外（`effective_kinds` が DM を外す）が
    // 破られても効く最終防壁で、`is_dm()` が DM の唯一の判定源（[`opencrab_nostr::DM_KINDS`]）。
    //
    // 公開リプライへ回さないのが要点: 以前 DM に kind:1 の公開リプライで返す事故があった
    // （復号できておらず中身は漏れなかったが、復号が直れば DM 本文が公開タイムラインに
    // 出ていた）。ここで return するので応答生成（＝返信 publish）自体が起きない。
    // 黙って捨てると届いていたことすら分からないので、送信者 pubkey と kind を INFO で残す。
    if event.is_dm() {
        info!(
            agent_id,
            sender = %event.pubkey,
            kind = event.kind,
            "nostr: dropping DM (kind 4/1059 are not handled — receive discarded, no reply; #514)"
        );
        return;
    }

    // agent 単位のセッション（**1 エージェント = 1 会話** / #323）。誰から来た受信も
    // ここへ落ちるので、エージェントは自分の発言も含めて 1 本の履歴として読める。
    // 誰の発言かは下の `sender_id`（= 相手の pubkey）が担う。
    let session_id = nostr_session_id(agent_id);

    // 会話履歴・転記に載せる本文（#282）。本文だけを記録していたため、次ターン以降の
    // エージェントは author の npub も note id も kind も参照できなかった（nostaro 本体
    // より劣化）。#272/#274 の画像アンカーと同じく、受信メタ情報を本文側に焼き込む。
    // 転記とエージェント向けで**同じ文字列**を使い、従来の非対称（転記にだけ kind が
    // 載る）を解消する。
    let inbound_text = event.inbound_text();

    runner.ensure_session(&session_id, &[agent_id.to_string()], "Nostr", "{}", "nostr");

    // #570: 会話履歴へ残す本文だけ、tool_result と同じ退避
    // （[`opencrab_actions::sanitize_tool_result_for_log`]）に乗せる。Nostr の受信本文は
    // relay が受け付けたサイズがそのまま入り、コード上の上限が無かった。超大受信が
    // 「直近ユーザー発言」枠で退避も予算も素通りし、単独で context 予算を食い潰す経路を塞ぐ。
    //
    // - 閾値・保存先・案内書式は tool_result と**同一**（新しい流儀を足さない）。退避先は
    //   エージェントのワークスペース `<root>/tmp/`（`ws_read` で読み返せる）。ファイル名の
    //   一意キーには tool_call_id の代わりに Nostr の event.id を使う。
    // - **閾値以下は完全な no-op**（本文を 1 バイトも変えない）: 本番最大の受信
    //   （6,761 字 ≒ 1,700 トークン < 2,500）はここを素通りする。
    // - 秘密（nsec）混入時のマスクも同じ経路で掛かる（防御的多層）。
    //
    // 転記（`relay_inbound_notification`）は**人間向けの生本文**のまま送る（下でそのまま
    // `inbound_text` を使う）。転記先はプラットフォーム側でサイズ頭打ちになり、退避案内を
    // 人が読むチャンネルへ流すのは不適切なため、退避は会話履歴の側だけに掛ける。
    let recorded_text = opencrab_actions::sanitize_tool_result_for_log(
        "nostr_inbound",
        &inbound_text,
        &session_id,
        &event.id,
        runner.agent_workspace_root(agent_id).as_deref(),
    );

    // #284 P0-3: 受信発言の記録失敗は握り潰さない。落ちた発言は会話履歴に現れず、
    // エージェントはその投稿を見ないまま応答することになる。
    let recorded = runner.record_inbound_message(
        opencrab_actions::TranscriptSource::Nostr,
        &opencrab_actions::InboundMessageRecord {
            session_id: &session_id,
            recipient_agent_id: agent_id,
            sender_id: &event.pubkey,
            sender_name: &event.author_label(),
            avatar_url: None,
            channel_id: None,
            pubkey: Some(&event.pubkey),
            text: &recorded_text,
            image_urls: &[],
        },
    );
    if !recorded {
        tracing::error!(
            session_id = %session_id,
            agent_id = %agent_id,
            "failed to persist an inbound Nostr message after retries; the agent will answer \
             WITHOUT ever seeing it. Check database health."
        );
    }

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
            // #282: エージェントの会話履歴に残るのと同一の本文（メタアンカー込み）。
            body = inbound_text,
        );
        runner.relay_inbound_notification(&target, relay_text);
    }

    // #282: 「返信先はこれ」という指示だけでなく、受信イベントの**事実**（誰の / どのノート /
    // どの kind）を明示する。すべて公開情報なので隠す理由はない（nsec は当然出さない）。
    let prompt_suffix = format!(
        "[Nostr] {author} さんの投稿への応答です。\n\
         - 送信者: {author_key}（pubkey={pubkey}）\n\
         - 対象ノート: {target}\n\
         - 種別: kind:{kind}（{label}）\n\
         返信するなら nostr_reply(target=\"{target}\") を使ってください（target は返信先ノート）。\
         種別的に本文返信が不自然なもの（リアクション等）や、返信不要なら \
         NO_REPLY とだけ答えてください。",
        author = event.author_label(),
        author_key = event.author_key(),
        pubkey = event.pubkey,
        target = event.reply_target(),
        kind = event.kind,
        label = event.inbound_kind_label(),
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
    // #323 / B2: このターンの返信相手（= 転記の speaker_id）。走行中注入をこの相手の
    // 連投だけに絞り、別相手の新着が reply_target と食い違う本文を公開リレーへ誤爆
    // させない（旧 per-相手 セッションの性質の復元）。
    let speaker_pubkey = event.pubkey.clone();
    let job_session_id = session_id.clone();
    // 呼び出し元は**発言者**から決める（#319）。以前は応答生成側が
    // `CallerIdentity::Agent` 固定で、オーナーが話しかけても外部の誰かが話しかけても
    // 同じ扱いだった（＝エージェントが Nostr 発のターンから自分の設定を変更できない）。
    //
    // 解決は `event.pubkey` を**持っているここ**で行う（同期 DB 読みのみ / await しない）。
    // 応答生成側で session_id から逆算すると、セッション規約を変えた瞬間に権限判定が
    // 壊れる。オーナー未設定・未登録なら従来どおり `Agent`（fail-closed）。
    let caller = runner.resolve_nostr_caller(agent_id, &event.pubkey);
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
                caller,
                opencrab_actions::LiveInboundScope::OnlySpeaker(speaker_pubkey),
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
        *self.self_pubkey.write().unwrap() = new_pubkey.clone();

        // 4) DB を最後に更新。失敗したら config/セルを旧状態へ巻き戻す（DB=旧 / config=新
        //    の不整合＝再起動で勝手に切替完了する事故を防ぐ）。
        if let Err(e) = self.runner.set_nostr_secret_key(agent_id, &nsec) {
            if let Err(re) = NostaroCli::materialize_config(agent_id, &old_secret, &relays, None) {
                error!(agent_id, error = %re, "nostr: identity 切替のロールバック（config復元）に失敗");
            }
            *self.self_pubkey.write().unwrap() = old_pubkey;
            return Err(e).context("DB の本鍵更新に失敗（設定を元に戻しました）");
        }

        // 5) #489: co_agent 逆引き表（`agent_nostr_config.self_pubkey`）も新鍵の pubkey へ
        //    揃える。**DB の secret_key を更新し切った後だけ**書く（secret_key が旧鍵へ
        //    ロールバックした経路ではここへ来ないので、逆引き表が新鍵を指して secret_key と
        //    食い違うことはない）。保存前に正規化する（起動時と同じ扱い。突合相手の author も
        //    正規化 hex）。書き込みに失敗しても切替自体は成立済み（config/DB の secret_key は
        //    新鍵）なので致命ではない: 逆引きが旧鍵のまま stale になるだけで fail-closed
        //    （誤許可はしない）。次回起動の `spawn_agent_gateway` が自 pubkey を再導出して直すので、
        //    ここは best-effort（ログのみ）。
        match crate::normalize_pubkey(&new_pubkey) {
            Some(hex) => {
                if let Err(e) = self.runner.set_nostr_self_pubkey(agent_id, &hex) {
                    warn!(agent_id, error = %e, "#489: identity 切替後の self_pubkey 書き戻しに失敗（co_agent 逆引きは次回起動まで stale・fail-closed）");
                }
            }
            None => {
                warn!(agent_id, "#489: identity 切替後の自 pubkey を正規化できず逆引き表を更新しなかった（co_agent は次回起動まで stale・fail-closed）");
            }
        }
        Ok(npub.to_string())
    }
}

/// gateway を停止する（handle abort + 採用 admin 除去）。稼働していなければ何もしない。
fn stop_gateway(gateways: &GatewayMap, admins: &AdminMap, agent_id: &str) {
    let handle = gateways.write().unwrap().remove(agent_id);
    // 採用 admin も一緒に外す（gateways と生死を揃える＝停止後に稼働中と誤判定させない）。
    admins.write().unwrap().remove(agent_id);
    if let Some(handle) = handle {
        // abort でループ frame を drop → 子 nostaro は kill_on_drop で kill される。
        handle.abort();
        info!(agent_id, "Per-agent Nostr gateway stopped");
    }
}

/// watch ループを起動して登録簿（gateways/admins）へ登録する**単一チョークポイント**。
///
/// `NostrGatewayManager::start_agent_gateway`（通常起動・restore）と identity 採用の
/// bootstrap（[`NostrIdentityProvisioner`]）が同じこの経路を通ることで、資格情報ガード
/// （空 nsec 拒否）・自己 pubkey 取得の fail-closed が呼び出し口によらず必ず効く
/// （PUT enabled=false→/start バイパス封じと同じ設計）。
#[allow(clippy::too_many_arguments)]
async fn spawn_agent_gateway<R: NostrAgentRunner>(
    gateways: &GatewayMap,
    admins: &AdminMap,
    runner: &R,
    cli: &NostaroCli,
    runtime: &Arc<NostrSessionRuntime>,
    agent_id: &str,
    secret_key: &str,
    config: NostrConfig,
) -> anyhow::Result<()> {
    // 資格情報のガード（#191 段階2 PR3）。空 / 空白だけの nsec では nostaro が動かないので、
    // materialize（0600 のファイル書き出し）や `pubkey` 取得より **手前**で拒否する。
    if secret_key.trim().is_empty() {
        return Err(opencrab_actions::StartDeclined::err(
            opencrab_actions::gateway_kinds::NOSTR,
            agent_id,
            "秘密鍵（nsec）が未設定です。先に鍵を生成してください",
        ));
    }
    // 【フィルタ空を拒否するガードはここに**無い**】（#271/#278）
    //
    // 以前は「author も keyword も無い＝全ノート洪水」として起動を拒否していた。旧 nostaro の
    // `watch --json` が mention-only を無視して kind:1 を全件購読していたので、当時は正しかった。
    // 新 nostaro では `--json` でも mention-only が既定で効き、`build_watch_command` は
    // `--no-mention-only` を渡さないので、**フィルタ未指定の購読は「自分宛の p タグのみ」＝
    // 最も狭い**。逆に keywords を足すほど（nostaro が keyword 用に kind 全体の購読を張るぶん）
    // 広くなる。旧ガードは一番狭い設定だけを拒否する裏返しの判定になっていたので撤去した。
    //
    // 洪水を防ぐ不変条件は `NostaroCli::build_watch_command` が持つ（`--no-mention-only` を
    // 渡さない / `--match=any` を明示する）。どちらもテストで固定している。

    stop_gateway(gateways, admins, agent_id);

    // nsec を含む config を 0600 で書き出す。
    NostaroCli::materialize_config(agent_id, secret_key, &config.effective_relays(), None)?;

    // 自分の pubkey は自己返信ループ防止に必須。取得できなければ **起動しない**
    // （fail-closed: 自己フィルタ無しで走ると keyword フィルタ時に自分の返信を拾って
    // 無限ループ＋LLM 支出になる）。
    let self_pubkey = cli
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

    // #489: 自 pubkey を co_agent 逆引き表（`agent_nostr_config.self_pubkey`）へ書き戻す。
    // 出所は自 secret_key から導出した自分の pubkey（受信著者ではない）＝信頼できる出所。
    // 起動/restore の度に走るので既存 agent もここで backfill される。
    //
    // **正規化してから保存する**（突合相手の author も `normalize_pubkey` で 64 桁小文字 hex に
    // 揃えて引くため）。`nostaro pubkey` は小文字 hex を返す前提だが、万一 npub / 大文字を
    // 返しても「黙って壊れた値」を保存しない（正規化不能なら保存を見送って警告）。書けなくても
    // 致命ではない（逆引き不可 → co_agent は fail-closed）ので best-effort（ログのみ）。
    match crate::normalize_pubkey(&self_pubkey) {
        Some(hex) => {
            if let Err(e) = runner.set_nostr_self_pubkey(agent_id, &hex) {
                warn!(agent_id, error = %e, "#489: self_pubkey の書き戻しに失敗（co_agent 逆引きは fail-closed のまま）");
            }
        }
        None => {
            warn!(agent_id, "#489: 自 pubkey を正規化できず逆引き表を更新しなかった（co_agent は fail-closed のまま）");
        }
    }

    let runner_c = runner.clone();
    let cli_c = cli.clone();
    let agent = agent_id.to_string();
    // self_pubkey は共有セル。identity 切替（本鍵採用）時に新 pubkey へ更新できる
    // ようにする（watch は鍵非依存なのでプロセス再起動不要）。
    let self_pubkey_cell = Arc::new(RwLock::new(self_pubkey));
    // identity 切替の実体（runner+cli+セルを capture）。稼働中の採用（ホットスワップ）で使う。
    let admin: Arc<dyn NostrIdentityAdmin> = Arc::new(LoopIdentityAdmin {
        runner: runner_c.clone(),
        cli: cli_c.clone(),
        self_pubkey: self_pubkey_cell.clone(),
    });
    // 採用 admin を登録簿へ（handle と同時に入れる＝稼働中の判定と生死が揃う）。
    admins
        .write()
        .unwrap()
        .insert(agent_id.to_string(), admin.clone());
    let runtime_c = runtime.clone();
    let handle = tokio::spawn(async move {
        run_nostr_loop(
            runner_c,
            cli_c,
            agent,
            config,
            self_pubkey_cell,
            admin,
            runtime_c,
        )
        .await;
    });

    gateways
        .write()
        .unwrap()
        .insert(agent_id.to_string(), handle);
    info!(agent_id, "Per-agent Nostr gateway started");
    Ok(())
}

/// 生成鍵の採用（identity 切替）capability の実体（#264）。
///
/// マネージャと**同じ登録簿**（`gateways` / `admins` の Arc）を共有する。判定は 2 モード:
/// - **稼働中** → per-agent admin で in-place ホットスワップ（self_pubkey セル更新・再接続
///   なし）。既存挙動をそのまま維持する。
/// - **未稼働（自己ブートストラップ）** → `agent_nostr_config` に鍵・リレー・**空フィルタ**
///   （＝nostaro の mention-only 既定に委ねて自分宛のみ / #271）を enabled=false で書き、
///   [`spawn_agent_gateway`] で起動＝接続、
///   成功後に enabled=true。これで「未設定エージェントが `nostr_generate_key`→
///   `nostr_switch_identity` を呼ぶだけで自力で載る」が成立する。
pub struct NostrIdentityProvisioner<R: NostrAgentRunner> {
    gateways: GatewayMap,
    admins: AdminMap,
    runner: R,
    cli: NostaroCli,
    runtime: Arc<NostrSessionRuntime>,
}

impl<R: NostrAgentRunner> NostrIdentityProvisioner<R> {
    /// bootstrap 採用で書き込む [`NostrConfig`] を組む。
    ///
    /// - **relays**: 既存設定があればそれを尊重、無ければ [`crate::config::DEFAULT_RELAYS`]。
    /// - **filter**: 既存設定を**そのまま**尊重する。無ければ**空**（＝自分宛のみ）。
    ///
    /// ## keyword を自動設定しない（#271）
    ///
    /// #264 の初版はここで `keywords=[自分の npub]` を自動設定していた。当時の
    /// `filter_is_unbounded()` ガードを通すためだったが、これは本文に npub 文字列を含む
    /// 投稿しか拾わないという条件を足すことになり、**e/p タグだけの返信（本文に npub を
    /// 含まない普通のリプライ）が丸ごと落ちていた**（実機で確認済み）。
    ///
    /// nostaro の `watch` は **mention-only 既定**で自分宛の p タグを購読するので、
    /// 「自分への言及だけ購読」は**フィルタを空にするだけで成立する**。opencrab 側で
    /// 条件を足すのは劣化にしかならないので外した。自分の投稿は watch ループが
    /// author 一致でスキップするので自己ループにもならない。
    fn bootstrap_config(&self, agent_id: &str) -> NostrConfig {
        let existing = self
            .runner
            .get_nostr_config(agent_id)
            .map(|r| crate::config_from_row(&r));
        let relays = existing
            .as_ref()
            .map(|c| c.relays.clone())
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| {
                crate::config::DEFAULT_RELAYS
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });
        // 運用者が設定済みならそのまま（勝手に足さない・勝手に外さない）。未設定なら空＝
        // 「自分宛のみ」。npub は使わない（#271: keyword 自動設定が返信を落としていた）。
        let filter = existing.map(|c| c.filter).unwrap_or_default();
        NostrConfig { relays, filter }
    }
}

#[async_trait::async_trait]
impl<R: NostrAgentRunner> opencrab_actions::GatewayIdentityProvisioning
    for NostrIdentityProvisioner<R>
{
    async fn adopt_identity(&self, agent_id: &str, npub: &str) -> anyhow::Result<String> {
        // 稼働中なら既存のホットスワップ経路（self_pubkey セルを in-place 更新・再接続なし）。
        let running_admin = self.admins.read().unwrap().get(agent_id).cloned();
        if let Some(admin) = running_admin {
            return admin.adopt_generated_identity(agent_id, npub).await;
        }

        // 未稼働＝自己ブートストラップ。生成鍵（自分のもの）の nsec を読む。存在チェックで
        // 「自分が生成した鍵のみ採用可」を担保。秘密鍵は外へ出さない・返さない。
        let nsec = NostaroCli::read_generated_key(agent_id, npub)?;
        let config = self.bootstrap_config(agent_id);

        // 1) agent_nostr_config を **enabled=false で先に書く**（順序ガード: 起動成功後に
        //    enabled=true。失敗時に「enabled だが未稼働」の不整合を残さない / manager.rs の
        //    「enabled を見ない」設計と整合）。
        let row = opencrab_db::queries::AgentNostrConfigRow {
            agent_id: agent_id.to_string(),
            secret_key: nsec.clone(),
            relays_json: serde_json::to_string(&config.relays).unwrap_or_else(|_| "[]".to_string()),
            filter_json: serde_json::to_string(&config.filter).unwrap_or_else(|_| "{}".to_string()),
            enabled: false,
        };
        self.runner.upsert_nostr_config(&row)?;

        // 2) 起動＝接続。失敗時は
        //    enabled=false のまま（inert: restore で起動されず、is_running も false）。
        spawn_agent_gateway(
            &self.gateways,
            &self.admins,
            &self.runner,
            &self.cli,
            &self.runtime,
            agent_id,
            &nsec,
            config,
        )
        .await?;

        // 3) 起動成功後に enabled=true（次回のプロセス再起動で restore_from_db が復元する）。
        self.runner.set_nostr_enabled(agent_id, true)?;
        Ok(npub.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Mutex;

    use opencrab_actions::webhook_target::WebhookConfig;
    use opencrab_actions::{CallerIdentity, RunRequest};
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
        /// 受信の転記先 session_id（session_id 規約の検証用 / #323）。
        recorded_sessions: Arc<Mutex<Vec<String>>>,
        /// 受信の発言者 id（1 セッションに混ざっても誰の発言か分かることの検証用 / #323）。
        recorded_speakers: Arc<Mutex<Vec<String>>>,
        /// 応答生成へ渡った session_id（#323）。
        run_sessions: Arc<Mutex<Vec<String>>>,
        /// 応答生成を**開始**した順（reply_target）。
        started: Arc<Mutex<Vec<String>>>,
        /// 応答生成を**完了**した順（reply_target = 実際に返信が飛ぶ順）。
        finished: Arc<Mutex<Vec<String>>>,
        /// 転記先の解決結果（#252）。`None` = 未設定（転記しない）。
        relay_target: Option<WebhookConfig>,
        /// 実際に転記口へ渡った本文（配送口のスパイ / #252）。
        relayed: Arc<Mutex<Vec<String>>>,
        /// 応答生成へ渡った system_prompt（= base + prompt_suffix / #282）。
        system_prompts: Arc<Mutex<Vec<String>>>,
        /// upsert された agent_nostr_config 行（自己ブートストラップ採用の検証 / #264）。
        upserted: Arc<Mutex<Vec<AgentNostrConfigRow>>>,
        /// set_nostr_enabled の呼び出し履歴（順序ガードの検証 / #264）。
        enabled_calls: Arc<Mutex<Vec<bool>>>,
        /// set_nostr_secret_key に渡った nsec（ホットスワップ経路の検証 / #264）。
        secret_sets: Arc<Mutex<Vec<String>>>,
        /// set_nostr_self_pubkey に渡った pubkey（co_agent 逆引き表の書き戻し検証 / #489）。
        self_pubkey_sets: Arc<Mutex<Vec<String>>>,
        /// get_nostr_config が返す既存行（`None`=未設定 / #264）。
        preset_config: Option<AgentNostrConfigRow>,
        /// `resolve_nostr_caller` が Owner と答える相手の pubkey（#319）。
        owner_pubkey: Option<String>,
        /// `resolve_nostr_caller` に渡された pubkey（発言者を見ているかの検証 / #319）。
        caller_queries: Arc<Mutex<Vec<String>>>,
        /// 応答生成へ渡った呼び出し元（#319）。
        callers: Arc<Mutex<Vec<CallerIdentity>>>,
        /// `agent_workspace_root` が返す退避先（#570）。`None`＝退避先なし。
        workspace_root: Option<std::path::PathBuf>,
    }

    impl SlowRunner {
        fn new(delay: Duration) -> Self {
            Self {
                delay,
                inflight: Arc::new(AtomicUsize::new(0)),
                max_inflight: Arc::new(AtomicUsize::new(0)),
                recorded: Arc::new(Mutex::new(Vec::new())),
                recorded_sessions: Arc::new(Mutex::new(Vec::new())),
                recorded_speakers: Arc::new(Mutex::new(Vec::new())),
                run_sessions: Arc::new(Mutex::new(Vec::new())),
                started: Arc::new(Mutex::new(Vec::new())),
                finished: Arc::new(Mutex::new(Vec::new())),
                relay_target: None,
                relayed: Arc::new(Mutex::new(Vec::new())),
                system_prompts: Arc::new(Mutex::new(Vec::new())),
                upserted: Arc::new(Mutex::new(Vec::new())),
                enabled_calls: Arc::new(Mutex::new(Vec::new())),
                secret_sets: Arc::new(Mutex::new(Vec::new())),
                self_pubkey_sets: Arc::new(Mutex::new(Vec::new())),
                preset_config: None,
                owner_pubkey: None,
                caller_queries: Arc::new(Mutex::new(Vec::new())),
                callers: Arc::new(Mutex::new(Vec::new())),
                workspace_root: None,
            }
        }

        /// 受信本文の退避先を仕込む（#570 の退避経路の検証用）。
        fn with_workspace_root(mut self, root: std::path::PathBuf) -> Self {
            self.workspace_root = Some(root);
            self
        }

        /// 「この pubkey がオーナー」という解決結果を仕込む（#319）。
        fn with_owner_pubkey(mut self, pubkey: &str) -> Self {
            self.owner_pubkey = Some(pubkey.to_string());
            self
        }

        /// get_nostr_config が返す既存設定を仕込む（ホットスワップ経路の検証 / #264）。
        fn with_preset_config(mut self, row: AgentNostrConfigRow) -> Self {
            self.preset_config = Some(row);
            self
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
            self.callers.lock().unwrap().push(req.caller.clone());
            self.run_sessions
                .lock()
                .unwrap()
                .push(req.session_id.clone());
            self.system_prompts
                .lock()
                .unwrap()
                .push(req.system_prompt.clone());
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

        fn build_agent_context(
            &self,
            _agent_id: &str,
            _caller: &CallerIdentity,
        ) -> (String, String) {
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
        ) -> bool {
            assert_eq!(source, opencrab_actions::TranscriptSource::Nostr);
            self.recorded.lock().unwrap().push(record.text.to_string());
            self.recorded_sessions
                .lock()
                .unwrap()
                .push(record.session_id.to_string());
            self.recorded_speakers
                .lock()
                .unwrap()
                .push(record.sender_id.to_string());
            true
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
        /// 「オーナーの pubkey なら Owner」を模す（#319。解決の実体は server 側でテスト）。
        /// 問い合わせられた pubkey を記録して、**発言者を見ているか**を検証できるようにする。
        fn resolve_nostr_caller(&self, _agent_id: &str, author_pubkey: &str) -> CallerIdentity {
            self.caller_queries
                .lock()
                .unwrap()
                .push(author_pubkey.to_string());
            match self.owner_pubkey.as_deref() {
                Some(owner) if owner == author_pubkey => CallerIdentity::Owner,
                _ => CallerIdentity::Agent,
            }
        }

        fn list_enabled_nostr_configs(&self) -> Vec<AgentNostrConfigRow> {
            Vec::new()
        }

        fn get_nostr_config(&self, _agent_id: &str) -> Option<AgentNostrConfigRow> {
            self.preset_config.clone()
        }

        fn set_nostr_secret_key(&self, _a: &str, s: &str) -> anyhow::Result<()> {
            self.secret_sets.lock().unwrap().push(s.to_string());
            Ok(())
        }

        fn set_nostr_self_pubkey(&self, _a: &str, pk: &str) -> anyhow::Result<()> {
            self.self_pubkey_sets.lock().unwrap().push(pk.to_string());
            Ok(())
        }

        fn upsert_nostr_config(&self, cfg: &AgentNostrConfigRow) -> anyhow::Result<()> {
            self.upserted.lock().unwrap().push(cfg.clone());
            Ok(())
        }

        fn set_nostr_enabled(&self, _agent_id: &str, enabled: bool) -> anyhow::Result<()> {
            self.enabled_calls.lock().unwrap().push(enabled);
            Ok(())
        }

        fn resolve_nostr_relay_target(&self, _agent_id: &str) -> Option<WebhookConfig> {
            self.relay_target.clone()
        }

        fn relay_inbound_notification(&self, _target: &WebhookConfig, text: String) {
            // 配送口のスパイ: 実際に転記へ回った本文を記録する（HTTP は出さない）。
            self.relayed.lock().unwrap().push(text);
        }

        fn agent_workspace_root(&self, _agent_id: &str) -> Option<std::path::PathBuf> {
            self.workspace_root.clone()
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
            self.feed_event(event(id, pubkey, content)).await;
        }

        /// 任意のイベントを1件流す（メタ情報の検証用 / #282）。
        async fn feed_event(&self, ev: NostrEvent) {
            self.feed_event_as(&self.agent_id, ev).await;
        }

        /// **別エージェント**として1件流す（= 別セッション / #323）。
        ///
        /// session が agent 単位になったので、「別セッション」を作る唯一の軸が
        /// エージェントになった。本番では `permits` / `queues` はエージェント毎に
        /// 作られるが（[`run_nostr_loop`]）、ここで見たいのは [`SessionQueues`] が
        /// 複数 session を持ったときの挙動なので、意図的に 1 束を共有して流す。
        async fn feed_event_as(&self, agent_id: &str, ev: NostrEvent) {
            handle_event(
                &self.runner,
                &self.cli,
                agent_id,
                &self.admin,
                &self.runtime,
                &self.permits,
                &self.queues,
                ev,
            )
            .await;
        }

        /// [`Self::feed`] の別エージェント版（#323）。
        async fn feed_as(&self, agent_id: &str, id: &str, pubkey: &str, content: &str) {
            self.feed_event_as(agent_id, event(id, pubkey, content))
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

    // ---- #319: 受信ターンの呼び出し元は発言者から決まる ----

    /// ダミー鍵（実在の pubkey は書かない）。
    const OWNER_PK: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const STRANGER_PK: &str = "0000000000000000000000000000000000000000000000000000000000000002";

    /// **本丸**: オーナーの pubkey から届いた受信ターンは `Owner` で走る。
    ///
    /// 以前は応答生成側が `CallerIdentity::Agent` 固定で、OWNER_ONLY / TRUSTED_ONLY の
    /// ツールが list にも dispatch にも出なかった（#319）。
    #[tokio::test]
    async fn inbound_from_owner_runs_as_owner() {
        let h = Harness::with_runner(
            "agent-caller",
            SlowRunner::new(Duration::from_millis(0)).with_owner_pubkey(OWNER_PK),
            4,
            8,
        );
        h.feed("evt-owner", OWNER_PK, "設定を変えて").await;
        assert!(h.wait_finished(1, Duration::from_secs(2)).await);

        assert_eq!(
            h.runner.callers.lock().unwrap().as_slice(),
            [CallerIdentity::Owner],
            "オーナー発の受信ターンが Owner で走っていない"
        );
        // 解決には**受信イベントの pubkey** を渡している（session_id ではない）。
        assert_eq!(
            h.runner.caller_queries.lock().unwrap().as_slice(),
            [OWNER_PK.to_string()]
        );
    }

    /// **本丸**: 他人の pubkey から届いたターンは `Agent` のまま（昇格しない）。
    #[tokio::test]
    async fn inbound_from_stranger_stays_agent() {
        let h = Harness::with_runner(
            "agent-caller",
            SlowRunner::new(Duration::from_millis(0)).with_owner_pubkey(OWNER_PK),
            4,
            8,
        );
        h.feed("evt-stranger", STRANGER_PK, "設定を変えて").await;
        assert!(h.wait_finished(1, Duration::from_secs(2)).await);

        assert_eq!(
            h.runner.callers.lock().unwrap().as_slice(),
            [CallerIdentity::Agent],
            "他人の pubkey が昇格した"
        );
    }

    /// オーナー未設定なら誰も Owner にならない（fail-closed）。
    #[tokio::test]
    async fn inbound_without_configured_owner_stays_agent() {
        // with_owner_pubkey を仕込まない＝解決側がオーナー無しと答える。
        let h = Harness::new("agent-caller", Duration::from_millis(0), 4, 8);
        h.feed("evt-1", OWNER_PK, "設定を変えて").await;
        assert!(h.wait_finished(1, Duration::from_secs(2)).await);

        assert_eq!(
            h.runner.callers.lock().unwrap().as_slice(),
            [CallerIdentity::Agent]
        );
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
            // 転記本文は「本文 + 受信メタアンカー」（#282）なので本文の先頭一致で見る。
            assert_eq!(
                SlowRunner::snapshot(&h.runner.recorded)
                    .iter()
                    .map(|t| t.lines().next().unwrap_or_default().to_string())
                    .collect::<Vec<_>>(),
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

    /// [#323] 相手が違っても受信は**同じ session**に落ちる（agent 単位で 1 会話）。
    ///
    /// 旧規約 `nostr-{agent}-{author_pubkey}` は会話を相手ごとに割っていたため、
    /// エージェントは「自分がさっき誰に何を言ったか」を跨いで見られず、同じ内容を
    /// 繰り返したり自分の発言と食い違うことを言った（#323）。Nostr のスレッドは
    /// そもそも多人数なので、「1 相手 = 1 会話」という前提自体が合っていない。
    ///
    /// **発言者の区別は session ではなく `speaker_id` が担う**。転記の `sender_id`
    /// （= 相手の pubkey）はイベントごとに入るので、1 本に混ざっても誰の発言かは
    /// 失われない（会話文字列は `[{speaker_id}]:` で出る）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn events_from_different_authors_share_one_session() {
        let h = Harness::new("agent-one-session", Duration::from_millis(5), 8, 32);

        h.feed("m1", "pk-alice", "1件目").await;
        h.feed("m2", "pk-bob", "2件目").await;
        assert!(
            h.wait_finished(2, Duration::from_secs(5)).await,
            "応答生成が完了しない"
        );

        let expected = vec!["nostr-agent-one-session".to_string(); 2];
        assert_eq!(
            SlowRunner::snapshot(&h.runner.recorded_sessions),
            expected,
            "相手が違っても転記先の session は 1 本"
        );
        assert_eq!(
            SlowRunner::snapshot(&h.runner.run_sessions),
            expected,
            "応答生成も同じ session で走る（履歴が揃う）"
        );
        // 1 本に混ざっても「誰の発言か」は転記の speaker_id で区別が付く。
        assert_eq!(
            SlowRunner::snapshot(&h.runner.recorded_speakers),
            vec!["pk-alice".to_string(), "pk-bob".to_string()],
            "発言者が session に潰されている（プロンプトで相手を区別できない）"
        );
    }

    /// [#323] 同一エージェントの応答生成は、**相手が違っても**直列化される。
    ///
    /// 「発言し終わるまで次の LLM を呼ばない」（オーナー方針）。直列化の鍵は
    /// `SessionRuntime` の session_id なので、session が agent 単位で 1 本になれば
    /// 追加の仕掛け無しにそのまま成り立つ。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn responses_of_one_agent_are_serialized_across_authors() {
        let h = Harness::new("agent-serial", Duration::from_millis(80), 8, 32);

        h.feed("s1", "pk-alice", "1").await;
        h.feed("s2", "pk-bob", "2").await;
        assert!(
            h.wait_finished(2, Duration::from_secs(5)).await,
            "応答生成が完了しない"
        );

        assert_eq!(
            h.runner.max_inflight.load(AtomicOrdering::SeqCst),
            1,
            "同一エージェントの応答生成が並行した（相手ごとに割れている）"
        );
        assert_eq!(
            SlowRunner::snapshot(&h.runner.finished),
            vec!["note1s1".to_string(), "note1s2".to_string()],
            "投入順どおりに 1 件ずつ返る"
        );
    }

    /// [P1 回帰 / #178] 受信ループは応答生成を await しない。
    ///
    /// 以前は `respond_serialized(...).await` をループ内で直接呼んでいたため、長い応答の
    /// あいだ**全セッション・全相手**の受信が止まった（`nostaro watch` の stdout も
    /// 読まれず滞留）。ここでは 2 件の `handle_event` が即座に返ること、かつ別セッション
    /// の応答が並行することを見る。
    ///
    /// #323 で session は agent 単位になったので、「別セッション」= 別エージェント。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn handle_event_does_not_block_the_receive_loop() {
        let h = Harness::new("agent-loop", Duration::from_millis(300), 8, 32);

        let started = std::time::Instant::now();
        // 別セッション（別エージェント）から 2 件。ループ相当の直列呼び出し。
        h.feed("e1", "pk-a", "1件目").await;
        h.feed_as("agent-loop-2", "e2", "pk-b", "2件目").await;
        let elapsed = started.elapsed();

        // ループは応答生成（300ms）を待たずに次へ進んでいる。
        assert!(
            elapsed < Duration::from_millis(150),
            "受信ループが応答生成でブロックしている: {elapsed:?}"
        );
        // 受信の転記はループ内で同期的に済んでいる（順序も保たれる）。
        // 本文の後ろに受信メタアンカーが付く（#282）ので先頭行で比べる。
        assert_eq!(
            SlowRunner::snapshot(&h.runner.recorded)
                .iter()
                .map(|t| t.lines().next().unwrap_or_default().to_string())
                .collect::<Vec<_>>(),
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
    /// を占有し、上限が埋まった時点でループ全体（＝全受信）が停止した。レビュアーの
    /// 実験と同型: permits=2 / 同一セッション 2 件 → 別セッション 1 件。別セッションの
    /// 応答生成が、詰まっているセッションの完了を待たずに始まることを見る。
    ///
    /// #323 で session は agent 単位になったので、「別セッション」= 別エージェント。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn permit_starvation_does_not_stall_the_receive_loop() {
        for trial in 0..3 {
            let h = Harness::new("agent-starve", Duration::from_millis(300), 2, 32);

            // 多弁なセッションが permit を使い切ろうとする。
            h.feed("s1", "pk-chatty", "1").await;
            h.feed("s2", "pk-chatty", "2").await;
            // 別セッション。ここでループが止まってはいけない。
            let started = std::time::Instant::now();
            h.feed_as("agent-starve-2", "s3", "pk-quiet", "3").await;
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
    ///
    /// #323 以降、別セッション = 別エージェント。同一セッションで流すと per-session
    /// 直列化だけで `max_inflight == 1` になり、**permit の有無を検知できない**
    /// （上限を消しても緑のままになる）ので、必ずセッションを分けて流す。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_responses_are_capped_by_permits() {
        let h = Harness::new("agent-cap", Duration::from_millis(80), 1, 32);

        let started = std::time::Instant::now();
        h.feed_as("agent-cap-1", "c1", "pk-a", "1").await;
        h.feed_as("agent-cap-2", "c2", "pk-b", "2").await;
        h.feed_as("agent-cap-3", "c3", "pk-c", "3").await;
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
    ///
    /// #323 でセッションが 1 本になっても回収が壊れないことを、複数セッション
    /// （= 複数エージェント）を並べたまま確かめる。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_session_queue_is_reclaimed() {
        let h = Harness::new("agent-reclaim", Duration::from_millis(5), 8, 32);
        h.feed("r1", "pk-a", "1").await;
        h.feed_as("agent-reclaim-2", "r2", "pk-b", "2").await;
        assert_eq!(
            h.queues.active_sessions(),
            2,
            "投入直後は session ごとにキューが存在する"
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

    /// [#514] DM（kind:4 / 1059）は会話へ入らない: 記録も応答生成も転記も起きない。
    ///
    /// テスト 1（会話へ入らない）＋ テスト 4 の対（通常 kind は従来どおり）。
    /// **変異確認**: `handle_event` 冒頭の `if event.is_dm() { return; }` を外すと、
    /// DM が record/started に現れてこのテストが赤くなる。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dm_is_dropped_before_entering_conversation() {
        const URL: &str = "https://discord.com/api/webhooks/1/tok";
        // 転記も有効にして「DM は転記もされない」まで見る。
        let runner = SlowRunner::new(Duration::from_millis(1)).with_relay_target(URL);
        let h = Harness::with_runner("agent-dm-drop", runner, 8, 32);

        for &kind in crate::event::DM_KINDS {
            let mut ev = rich_event(kind);
            ev.id = format!("dm{kind}");
            h.feed_event(ev).await;
        }

        // 会話履歴に入らない（記録ゼロ）。
        assert!(
            SlowRunner::snapshot(&h.runner.recorded).is_empty(),
            "DM は会話履歴へ記録されない"
        );
        // 転記（Discord webhook）へも回らない。
        assert!(
            SlowRunner::snapshot(&h.runner.relayed).is_empty(),
            "DM は Discord へ転記されない"
        );
        // 応答生成が起きない＝返信 publish 経路に一切入らない（テスト 2: kind:1 の
        // 公開リプライで返した事故の回帰）。少し待っても started は空のまま。
        assert!(
            !h.wait_finished(1, Duration::from_millis(200)).await,
            "DM で応答生成が走ってはいけない"
        );
        assert!(
            SlowRunner::snapshot(&h.runner.started).is_empty(),
            "DM で run_agent_response（＝返信 publish 経路）が呼ばれない"
        );

        // 対照: 通常の kind:1 は従来どおり記録され、応答生成が走る（テスト 4）。
        h.feed_event(rich_event(1)).await;
        assert!(
            h.wait_finished(1, Duration::from_secs(2)).await,
            "通常ノートは従来どおり処理される"
        );
        assert_eq!(
            SlowRunner::snapshot(&h.runner.recorded).len(),
            1,
            "通常ノートは 1 件記録される（DM は数に入らない）"
        );
    }

    /// メタ情報の検証用イベント（npub / note_id / kind を明示的に持つ / #282）。
    fn rich_event(kind: u32) -> NostrEvent {
        NostrEvent {
            id: "deadbeefid".to_string(),
            pubkey: "0011223344556677".to_string(),
            npub: Some("npub1author".to_string()),
            note_id: Some("note1target".to_string()),
            author_name: Some("kojira".to_string()),
            created_at: 1_700_000_000,
            kind,
            content: "こんにちは".to_string(),
            tags: Vec::new(),
        }
    }

    /// [#282] 会話履歴に残る本文へ、author の npub / note id / kind が焼き込まれる。
    /// 本文だけを記録していた劣化（nostaro 本体より情報が少ない）の回帰防止。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inbound_record_carries_author_note_and_kind() {
        let h = Harness::new("agent-meta", Duration::from_millis(1), 8, 32);
        h.feed_event(rich_event(1)).await;

        let recorded = SlowRunner::snapshot(&h.runner.recorded);
        assert_eq!(recorded.len(), 1);
        let text = &recorded[0];
        assert!(text.contains("こんにちは"), "本文が残る: {text}");
        assert!(
            text.contains("npub1author"),
            "author の npub が残る: {text}"
        );
        assert!(text.contains("note1target"), "note id が残る: {text}");
        assert!(text.contains("kind:1"), "kind が残る: {text}");
        assert!(text.contains("メンション"), "種別ラベルが残る: {text}");
    }

    /// [#282] npub / note_id が無い（None）受信でも、アンカーは壊れず hex へフォールバック
    /// する（空の `from=` / `target=` が並ばない）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inbound_record_anchor_survives_missing_optional_fields() {
        let h = Harness::new("agent-meta-none", Duration::from_millis(1), 8, 32);
        let mut ev = rich_event(1);
        ev.npub = None;
        ev.note_id = None;
        h.feed_event(ev).await;

        let recorded = SlowRunner::snapshot(&h.runner.recorded);
        let text = &recorded[0];
        assert!(
            !text.contains("from=]") && !text.contains("from= "),
            "空の from= が残らない: {text}"
        );
        assert!(
            text.contains("from=0011223344556677"),
            "pubkey へフォールバックする: {text}"
        );
        assert!(
            text.contains("target=deadbeefid"),
            "hex id へフォールバックする: {text}"
        );
    }

    /// [#282] `prompt_suffix` にも npub / pubkey / note id / kind が事実として載る
    /// （「target=… を使え」という指示だけだった劣化の回帰防止）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_suffix_carries_author_note_and_kind() {
        let h = Harness::new("agent-suffix", Duration::from_millis(1), 8, 32);
        h.feed_event(rich_event(7)).await;
        assert!(
            h.wait_finished(1, Duration::from_secs(5)).await,
            "応答生成が完了しない"
        );

        let prompts = SlowRunner::snapshot(&h.runner.system_prompts);
        assert_eq!(prompts.len(), 1);
        let p = &prompts[0];
        assert!(p.contains("npub1author"), "npub が載る: {p}");
        assert!(p.contains("0011223344556677"), "pubkey が載る: {p}");
        assert!(p.contains("note1target"), "note id が載る: {p}");
        assert!(p.contains("kind:7"), "kind が載る: {p}");
        assert!(p.contains("リアクション"), "種別ラベルが載る: {p}");
        assert!(
            p.contains("nostr_reply(target=\"note1target\")"),
            "従来の返信指示も残る: {p}"
        );
    }

    /// [#282] 転記（Discord webhook）とエージェント向けの記録は**同じ本文**を出す
    /// （転記にだけ kind が載る非対称の解消）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_and_agent_record_carry_the_same_information() {
        const URL: &str = "https://discord.com/api/webhooks/1/tok";
        let runner = SlowRunner::new(Duration::from_millis(1)).with_relay_target(URL);
        let h = Harness::with_runner("agent-parity", runner, 8, 32);

        h.feed_event(rich_event(1)).await;

        let recorded = SlowRunner::snapshot(&h.runner.recorded);
        let relayed = SlowRunner::snapshot(&h.runner.relayed);
        assert_eq!(relayed.len(), 1);
        assert!(
            relayed[0].contains(&recorded[0]),
            "転記本文がエージェントの記録本文を丸ごと含む: relayed={} recorded={}",
            relayed[0],
            recorded[0]
        );
        for needle in ["npub1author", "note1target", "kind:1"] {
            assert!(
                relayed[0].contains(needle) && recorded[0].contains(needle),
                "{needle} が両方に載る"
            );
        }
    }

    /// [#570] トークン上限未満の受信は退避を**完全に素通り**する: 会話履歴へ残る本文は
    /// 生の `inbound_text` と 1 バイトも変わらず、ワークスペースに退避ファイルも作られない。
    /// 閾値以下 no-op の回帰防止。
    ///
    /// Nostr 受信（source=nostr / log_type=speech）の実測最大は **1,959 字 / 2,179 バイト**
    /// （288 行・2,000 字超は 0 行）。ここで使う ASCII 主体 **6,761 字**は実測最大ではなく、
    /// **no-op の回帰用に十分大きい合成値**（`o200k_base` で約 1,700 トークン < 2,500）。
    /// 純粋なかな 6,761 字は ~1 字/トークンで上限を超えてしまうため、「上限未満だが実測最大より
    /// 十分大きい」を作れる ASCII 主体（~4 字/トークン）で組む。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inbound_below_limit_is_recorded_verbatim() {
        // no-op 回帰用の合成本文: ASCII 主体 6,761 字（≒ 1,700 トークン < 2,500）。
        // 実測の Nostr 受信最大（1,959 字）より十分大きく、かつ上限未満に収まる。
        let content: String = "the nostaro bot posts publicly. "
            .repeat(220)
            .chars()
            .take(6_761)
            .collect();
        assert_eq!(
            content.chars().count(),
            6_761,
            "合成本文の文字数がズレている"
        );
        let ev = event("prodmax", "0011223344556677", &content);
        // 前提: この本文はトークン上限未満（退避されない領域）。
        assert!(
            opencrab_core::tokens::estimate_tokens(&ev.inbound_text())
                < opencrab_actions::TOOL_RESULT_TOKEN_LIMIT,
            "前提が崩れている: 合成本文（ASCII 主体 6,761 字）が上限を超えた"
        );
        let expected = ev.inbound_text();

        // 退避先（ワークスペース）を与えても、閾値以下なら 1 件も書かれない。
        let dir = tempfile::tempdir().unwrap();
        let runner =
            SlowRunner::new(Duration::from_millis(1)).with_workspace_root(dir.path().into());
        let h = Harness::with_runner("agent-570-noop", runner, 8, 32);
        h.feed_event(ev).await;

        let recorded = SlowRunner::snapshot(&h.runner.recorded);
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0], expected,
            "閾値以下の受信は生本文のまま記録される（no-op）"
        );
        // 発言者識別子（sender_id）は退避と無関係に不変（#501 の除外条件に巻き込まれない）。
        assert_eq!(
            SlowRunner::snapshot(&h.runner.recorded_speakers),
            vec!["0011223344556677".to_string()],
            "speaker_id は退避経路で変わらない"
        );
        // 退避ファイルは作られない。
        let tmp = dir.path().join("tmp");
        let offloaded = tmp.exists()
            && tmp
                .read_dir()
                .map(|mut d| d.next().is_some())
                .unwrap_or(false);
        assert!(!offloaded, "閾値以下なのに退避ファイルが作られた");
    }

    /// [#570] 閾値を超える受信は、tool_result と同じ仕組みでワークスペースへ退避され、
    /// 会話履歴には生データを 1 バイトも含まないメタ案内だけが残る。退避ファイルは
    /// `<workspace>/tmp/` にあり（`ws_read` で読み返せる）、全文が入っている。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inbound_over_limit_is_offloaded_to_workspace() {
        // 上限（2,500 トークン）を確実に超える本文。
        let content = "needle-token ".repeat(6_000);
        let ev = event("bigid", "aabbccddeeff0011", &content);
        let full = ev.inbound_text();
        assert!(
            opencrab_core::tokens::estimate_tokens(&full)
                >= opencrab_actions::TOOL_RESULT_TOKEN_LIMIT,
            "前提が崩れている: 上限を超えていない"
        );

        let dir = tempfile::tempdir().unwrap();
        let runner =
            SlowRunner::new(Duration::from_millis(1)).with_workspace_root(dir.path().into());
        let h = Harness::with_runner("agent-570-big", runner, 8, 32);
        h.feed_event(ev).await;

        let recorded = SlowRunner::snapshot(&h.runner.recorded);
        assert_eq!(recorded.len(), 1);
        let text = &recorded[0];
        // 生データは 1 バイトも会話履歴へ入らない。
        assert!(
            !text.contains("needle-token"),
            "生データが会話履歴に混ざった: {text}"
        );
        // tool_result と同じ案内書式（退避先パス入り）。
        assert!(text.contains("withheld"), "案内書式が既存と違う: {text}");
        assert!(text.contains("tmp/"), "退避先パスが案内に無い: {text}");
        // speaker_id は不変。
        assert_eq!(
            SlowRunner::snapshot(&h.runner.recorded_speakers),
            vec!["aabbccddeeff0011".to_string()],
        );
        // 退避ファイルが 1 つでき、全文（生の inbound_text）が入っている。
        let tmp = dir.path().join("tmp");
        let files: Vec<_> = tmp.read_dir().unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(files.len(), 1, "退避ファイルが 1 件だけできる");
        let saved = std::fs::read_to_string(&files[0]).unwrap();
        assert_eq!(
            saved, full,
            "退避ファイルに全文が入る（ws_read で読み返せる）"
        );
    }

    /// [#570] 退避先（workspace_root）が無い／解決できない場合でも、閾値超の生データを
    /// 会話履歴へ丸ごと入れない。「保存できず捨てた」と分かる案内だけを残す（fail-safe）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inbound_over_limit_without_workspace_is_not_dumped_raw() {
        let content = "secret-body ".repeat(6_000);
        let ev = event("nows", "0011223344556677", &content);
        // workspace_root を仕込まない（= agent_workspace_root は None）。
        let h = Harness::new("agent-570-nows", Duration::from_millis(1), 8, 32);
        h.feed_event(ev).await;

        let recorded = SlowRunner::snapshot(&h.runner.recorded);
        assert_eq!(recorded.len(), 1);
        let text = &recorded[0];
        assert!(
            !text.contains("secret-body"),
            "退避先が無いのに生データが会話履歴へ流れた: {text}"
        );
        assert!(text.contains("could not be saved"), "{text}");
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

    /// pubkey を返す fake nostaro（実リレーへは繋がない / #264）。
    ///
    /// `pubkey` サブコマンドのときだけ `pubkey_out` を stdout に返す。それ以外（`watch`
    /// など）は即終了する（受信ループは EOF → backoff で再試行するので handle は生き続け、
    /// `is_running` は true を保つ）。`pubkey_out` が空なら pubkey も空を返す（起動失敗を模す）。
    fn fake_nostaro(pubkey_out: &str) -> (tempfile::TempDir, NostaroCli) {
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = pubkey ]; then\n    printf '%s' '{pubkey_out}'\n    exit 0\n  fi\ndone\nexit 0\n"
        );
        let script = crate::test_support::write_fake_nostaro(dir.path(), &body);
        let cli = NostaroCli::new().with_binary_path(script.to_string_lossy().to_string());
        (dir, cli)
    }

    /// [#264] 未設定エージェントが自力で採用＝接続する。`nostr_switch_identity`（採用）を
    /// 未稼働状態で呼ぶと、鍵・DEFAULT リレー・**空フィルタ**を enabled=false で書き、
    /// ゲートウェイを起動して is_running=true にし、成功後に enabled=true にする
    /// （順序ガード）。
    ///
    /// [#271] フィルタは**空**であること。旧実装は `keywords=[自分の npub]` を自動設定して
    /// おり、本文に npub 文字列を含まない e/p タグだけの返信を落としていた。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adopt_identity_bootstraps_unconfigured_agent_and_connects() {
        use opencrab_actions::GatewayIdentityProvisioning;

        let agent = "agent-bootstrap-264";
        let npub = "npub1selfbootstrap";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());

        // 自分の生成鍵を保存（read_generated_key の存在チェック＝「自分の鍵のみ」を満たす）。
        NostaroCli::save_generated_key(
            agent,
            &crate::cli::GeneratedKey {
                nsec: "nsec1bootstrapsecret".to_string(),
                npub: npub.to_string(),
                pubkey: "hexpub".to_string(),
            },
        )
        .unwrap();

        let runner = SlowRunner::new(Duration::from_millis(1));
        let (_fake, cli) = fake_nostaro("selfpubkeyhex");
        let mgr = NostrGatewayManager::new(runner.clone()).with_cli(cli);

        assert!(!mgr.is_running(agent), "採用前は未稼働（未設定）");

        let adopted = mgr
            .identity_provisioner()
            .adopt_identity(agent, npub)
            .await
            .unwrap();
        assert_eq!(adopted, npub);
        // 返り値は npub のみ（nsec を出さない）。
        assert!(!adopted.contains("nsec"), "nsec を返さない");

        // 起動して接続済み（配送対象になれる状態）。
        assert!(
            mgr.is_running(agent),
            "採用で自力接続する（is_running=true）"
        );

        // upsert された config: 鍵＋DEFAULT relays＋空フィルタ、enabled=false。
        let upserted = runner.upserted.lock().unwrap().clone();
        assert_eq!(upserted.len(), 1, "config を 1 回 upsert する");
        let row = &upserted[0];
        assert_eq!(row.secret_key, "nsec1bootstrapsecret");
        assert!(!row.enabled, "先に enabled=false で書く（順序ガード）");
        let cfg = crate::config_from_row(row);
        assert!(
            cfg.filter.keywords.is_empty(),
            "[#271] keyword を自動設定しない（本文一致の条件を足すと p/e タグだけの返信が落ちる）: {:?}",
            cfg.filter.keywords
        );
        assert!(
            cfg.filter.authors.is_empty(),
            "author も自動設定しない: {:?}",
            cfg.filter.authors
        );
        assert!(
            !cfg.watches_beyond_self_mentions(),
            "上乗せ条件無し＝nostaro の mention-only 既定で自分宛のみを購読する"
        );
        assert_eq!(
            cfg.effective_relays(),
            crate::config::DEFAULT_RELAYS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            "未設定なら DEFAULT リレー"
        );

        // 起動成功後にだけ enabled=true。
        assert_eq!(
            *runner.enabled_calls.lock().unwrap(),
            vec![true],
            "起動成功後に enabled=true（1 回だけ）"
        );

        mgr.stop_agent_gateway(agent).await;
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#271] 運用者が明示した絞り込みは採用時に**そのまま**残す。
    ///
    /// 自動 keyword は付けない（前のテスト）が、逆に運用者が設定した keywords/authors を
    /// 勝手に外しもしない。relays も既存を継承する。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adopt_identity_bootstrap_keeps_operator_configured_filter() {
        use opencrab_actions::GatewayIdentityProvisioning;

        let agent = "agent-bootstrap-271-operator";
        let npub = "npub1operatorset";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        NostaroCli::save_generated_key(
            agent,
            &crate::cli::GeneratedKey {
                nsec: "nsec1operatorsecret".to_string(),
                npub: npub.to_string(),
                pubkey: "hexpub".to_string(),
            },
        )
        .unwrap();

        // 未稼働だが設定行はある（運用者がダッシュボードで絞り込みだけ入れた状態）。
        let existing = AgentNostrConfigRow {
            agent_id: agent.to_string(),
            secret_key: String::new(),
            relays_json: r#"["wss://relay.example"]"#.to_string(),
            filter_json: r#"{"authors":["npub1watched"],"keywords":["opencrab"],"kinds":[1,7]}"#
                .to_string(),
            enabled: false,
        };
        let runner = SlowRunner::new(Duration::from_millis(1)).with_preset_config(existing);
        let (_fake, cli) = fake_nostaro("selfpubkeyhex");
        let mgr = NostrGatewayManager::new(runner.clone()).with_cli(cli);

        mgr.identity_provisioner()
            .adopt_identity(agent, npub)
            .await
            .unwrap();

        let upserted = runner.upserted.lock().unwrap().clone();
        let cfg = crate::config_from_row(&upserted[0]);
        assert_eq!(
            cfg.filter.keywords,
            vec!["opencrab".to_string()],
            "運用者の keyword を保つ"
        );
        assert_eq!(
            cfg.filter.authors,
            vec!["npub1watched".to_string()],
            "運用者の author を保つ"
        );
        assert_eq!(cfg.filter.kinds, vec![1, 7], "運用者の kind を保つ");
        assert!(
            !cfg.filter.keywords.contains(&npub.to_string()),
            "自分の npub を勝手に足さない"
        );
        assert_eq!(
            cfg.effective_relays(),
            vec!["wss://relay.example".to_string()]
        );

        mgr.stop_agent_gateway(agent).await;
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#264 / 配送誤爆防止] 起動に失敗したら未接続のまま（is_running=false）で、
    /// enabled=true にしない（「enabled だが未稼働」の不整合＝配送誤爆を残さない）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adopt_identity_failure_leaves_agent_disconnected_and_disabled() {
        use opencrab_actions::GatewayIdentityProvisioning;

        let agent = "agent-bootstrap-fail-264";
        let npub = "npub1failboot";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
        NostaroCli::save_generated_key(
            agent,
            &crate::cli::GeneratedKey {
                nsec: "nsec1failsecret".to_string(),
                npub: npub.to_string(),
                pubkey: "x".to_string(),
            },
        )
        .unwrap();

        let runner = SlowRunner::new(Duration::from_millis(1));
        // pubkey を返さない fake → 起動が pubkey ガード（fail-closed）で失敗する。
        let (_fake, cli) = fake_nostaro("");
        let mgr = NostrGatewayManager::new(runner.clone()).with_cli(cli);

        let res = mgr.identity_provisioner().adopt_identity(agent, npub).await;
        assert!(res.is_err(), "pubkey 取得不可なら採用は失敗する");

        assert!(
            !mgr.is_running(agent),
            "起動失敗なら is_running=false（配送対象に数えない）"
        );
        assert!(
            !runner.enabled_calls.lock().unwrap().contains(&true),
            "起動失敗時に enabled=true にしない（不整合を残さない）"
        );

        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#264 回帰] 稼働中エージェントの採用は**既存のホットスワップ経路**を通る
    /// （bootstrap の upsert / enabled 書き込みをせず、本鍵だけ差し替える＝再接続なし）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adopt_identity_uses_hotswap_when_gateway_running() {
        use opencrab_actions::GatewayIdentityProvisioning;

        let agent = "agent-hotswap-264";
        let npub_new = "npub1hotswapnew";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());

        // 稼働中エージェント: 運用者が設定した既存フィルタを持つ（ホットスワップは既存 relays を継承）。
        let existing = AgentNostrConfigRow {
            agent_id: agent.to_string(),
            secret_key: "nsec1old".to_string(),
            relays_json: r#"["wss://yabu.me"]"#.to_string(),
            filter_json: r#"{"keywords":["opencrab"]}"#.to_string(),
            enabled: true,
        };
        let runner = SlowRunner::new(Duration::from_millis(1)).with_preset_config(existing);
        // #489: fake nostaro は自 pubkey を **大文字 hex** で返す。逆引き表へは保存前に
        // `normalize_pubkey` を通した **小文字 hex** が入る（突合相手の author も正規化 hex）
        // ことを、起動時・identity 切替の両経路で固定する。
        let pubkey_upper = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789";
        let pubkey_lower = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let (_fake, cli) = fake_nostaro(pubkey_upper);
        let mgr = NostrGatewayManager::new(runner.clone()).with_cli(cli);

        // 稼働させる（admin が admins 登録簿へ入る）。
        let configured = crate::config::NostrConfig {
            relays: vec!["wss://yabu.me".to_string()],
            filter: crate::config::NostrFilter {
                authors: vec![],
                keywords: vec!["opencrab".to_string()],
                kinds: vec![],
            },
        };
        mgr.start_agent_gateway(agent, "nsec1old", configured)
            .await
            .unwrap();
        assert!(mgr.is_running(agent));

        // 新しい生成鍵を保存して採用。
        NostaroCli::save_generated_key(
            agent,
            &crate::cli::GeneratedKey {
                nsec: "nsec1newhot".to_string(),
                npub: npub_new.to_string(),
                pubkey: "y".to_string(),
            },
        )
        .unwrap();

        let adopted = mgr
            .identity_provisioner()
            .adopt_identity(agent, npub_new)
            .await
            .unwrap();
        assert_eq!(adopted, npub_new);

        // ホットスワップ経路: bootstrap の upsert も enabled 書き込みもしない。
        assert!(
            runner.upserted.lock().unwrap().is_empty(),
            "稼働中はホットスワップ（config を upsert しない）"
        );
        assert!(
            runner.enabled_calls.lock().unwrap().is_empty(),
            "ホットスワップは enabled を触らない"
        );
        // 本鍵だけ差し替える（set_nostr_secret_key に新 nsec）。
        assert_eq!(
            *runner.secret_sets.lock().unwrap(),
            vec!["nsec1newhot".to_string()],
            "ホットスワップは本鍵だけ差し替える"
        );
        // #489: 自 pubkey は co_agent 逆引き表へ書き戻される（起動時 + identity 切替時の 2 回）。
        // どちらも fake nostaro の pubkey 出力（大文字 hex）を正規化した **小文字 hex**。
        // 切替でも stale にならない。
        assert_eq!(
            *runner.self_pubkey_sets.lock().unwrap(),
            vec![pubkey_lower.to_string(), pubkey_lower.to_string()],
            "起動時と identity 切替時に self_pubkey を正規化して書き戻す（#489）"
        );
        assert!(
            mgr.is_running(agent),
            "ホットスワップは再接続しない（稼働継続）"
        );

        mgr.stop_agent_gateway(agent).await;
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

    /// [#489] 自 pubkey が **正規化できない値**（npub でも 64 桁 hex でもない）なら、逆引き表へ
    /// **保存しない**（黙って壊れた値を入れない）。突合相手の author は `normalize_pubkey` 済みの
    /// 小文字 hex なので、生値を入れると必ず食い違って co_agent が静かに fail-closed で死ぬ
    /// ＝ #489 と同じ症状になる。それを防ぐ None 経路の回帰。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn self_pubkey_not_saved_when_unnormalizable() {
        let agent = "agent-badpub-489";
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());

        let existing = AgentNostrConfigRow {
            agent_id: agent.to_string(),
            secret_key: "nsec1old".to_string(),
            relays_json: r#"["wss://yabu.me"]"#.to_string(),
            filter_json: r#"{"keywords":["opencrab"]}"#.to_string(),
            enabled: true,
        };
        let runner = SlowRunner::new(Duration::from_millis(1)).with_preset_config(existing);
        // 64 桁 hex でも npub でもない非空出力 → pubkey 取得ガードは通るが normalize_pubkey は None。
        let (_fake, cli) = fake_nostaro("not-a-valid-pubkey");
        let mgr = NostrGatewayManager::new(runner.clone()).with_cli(cli);

        let configured = crate::config::NostrConfig {
            relays: vec!["wss://yabu.me".to_string()],
            filter: crate::config::NostrFilter {
                authors: vec![],
                keywords: vec!["opencrab".to_string()],
                kinds: vec![],
            },
        };
        mgr.start_agent_gateway(agent, "nsec1old", configured)
            .await
            .unwrap();

        assert!(
            mgr.is_running(agent),
            "自 pubkey が正規化不能でも gateway 自体は起動する（自己スキップは生値で機能する）"
        );
        assert!(
            runner.self_pubkey_sets.lock().unwrap().is_empty(),
            "#489: 正規化できない自 pubkey は逆引き表へ保存しない（黙って壊れた値を入れない）"
        );

        mgr.stop_agent_gateway(agent).await;
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }
}
