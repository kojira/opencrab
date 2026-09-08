//! Per-agent Nostr V3 core manager.
//!
//! External `nostr-gateway` processes own relay subscriptions and final-speech delivery. This
//! manager keeps the authoritative allow-set, provisions V3 instance/binding rows, and exposes
//! identity/key capabilities. It contains no in-process relay watch or delivery fallback.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use anyhow::Context;

use crate::adapter::{AllowSetStore, AllowSources};
use crate::cli::NostaroCli;
use crate::config::NostrConfig;
use crate::identity::NostrIdentityAdmin;
use crate::runner::NostrAgentRunner;
use crate::session::{NostrSessionRuntime, NOSTR_SESSION_PREFIX};
use crate::watch_policy::watch_subscribe_config;

mod allow_sources;
mod identity_admin;
mod identity_provisioner;

use allow_sources::*;
use identity_admin::*;
pub use identity_provisioner::NostrIdentityProvisioner;

/// watch ループが握る self_pubkey の共有セル（identity 切替で更新可能）。
type SelfPubkey = Arc<RwLock<String>>;

/// 元栓の許可集合の共有セル。判定本体は [`crate::adapter::AllowSources`]。
///
/// `self_pubkey` と同じく watch ループが握り、kind:3 差し替えや
/// trusted_users / owner / co_agent の更新へ追従するため**定期更新でこのセルを差し替える**
/// （[`run_watch_once`] の更新経路）。ゲート判定はこのセルを読むだけ。
type AllowGate = Arc<RwLock<AllowSources>>;

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

/// V3 の instance/binding 敷設。session 不在・membership 不一致は fail-loud。
pub type NostrProvisionFn = Arc<
    dyn Fn(&str, &str, &NostrConfig, &[opencrab_db::queries::SessionWatchRow]) -> anyhow::Result<()>
        + Send
        + Sync,
>;

/// 停止後の identity 切替。config を書き revision を +1 する。
pub type NostrReviseFn = Arc<
    dyn Fn(
            &str,
            &str,
            &NostrConfig,
            &[opencrab_db::queries::SessionWatchRow],
        ) -> anyhow::Result<u64>
        + Send
        + Sync,
>;

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
    /// #588 TimedFire / #603: 時刻発火の受け口を登録する登録簿（**必須**・`new` の引数）。
    /// Option + builder だと配線し忘れてもコンパイルが通り、実際 #602 で忘れて本番が止まった。
    timed_fire_router: Arc<opencrab_actions::TimedFireRouter>,
    /// 元栓の共有ストア。V3 said も同じ判断を読む。
    allow_store: AllowSetStore,
    /// V3 のときだけ呼ぶ binding 敷設。
    provisioner: Option<NostrProvisionFn>,
    /// V3 identity 切替の revision 更新。
    reviser: Option<NostrReviseFn>,
}

impl<R: NostrAgentRunner> NostrGatewayManager<R> {
    /// `timed_fire_router` は**必須**。scheduler の時刻発火をこのマネージャの per-agent ループへ
    /// 届けるための受け口レジストリで、渡さないと発火が届かない（#603。型で強制して #602 の再発を防ぐ）。
    pub fn new(runner: R, timed_fire_router: Arc<opencrab_actions::TimedFireRouter>) -> Self {
        // #588 Stage 2: watch ループ（inbound）と完了 sink（resume）が使う per-session 直列化を、
        // heartbeat・scheduler・Discord 受信ループと**同じ** `SessionLocks` 実体へ寄せる。
        // runner（= server の AppState）が持つ共有ロックを注入する（registry は従来どおり
        // このランタイム固有）。runner を move する前に取り出す。
        let runtime = Arc::new(NostrSessionRuntime::with_locks(runner.session_locks()));
        Self {
            gateways: Arc::new(RwLock::new(HashMap::new())),
            admins: Arc::new(RwLock::new(HashMap::new())),
            runner,
            cli: NostaroCli::new(),
            runtime,
            timed_fire_router,
            allow_store: AllowSetStore::default(),
            provisioner: None,
            reviser: None,
        }
    }

    pub fn with_cli(mut self, cli: NostaroCli) -> Self {
        self.cli = cli;
        self
    }

    pub fn with_provisioner(mut self, provisioner: NostrProvisionFn) -> Self {
        self.provisioner = Some(provisioner);
        self
    }

    pub fn with_reviser(mut self, reviser: NostrReviseFn) -> Self {
        self.reviser = Some(reviser);
        self
    }

    pub fn allow_store(&self) -> &AllowSetStore {
        &self.allow_store
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
            self.timed_fire_router.clone(),
            self.allow_store.clone(),
            self.provisioner.clone(),
            self.reviser.clone(),
        )
        .await
    }

    pub async fn stop_agent_gateway(&self, agent_id: &str) {
        stop_gateway(
            &self.gateways,
            &self.admins,
            &self.timed_fire_router,
            &self.allow_store,
            agent_id,
        );
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
    /// bootstrap 起動・稼働中判定・V3 の停止→revision→再起動が本体と一貫する。
    pub fn identity_provisioner(&self) -> Arc<NostrIdentityProvisioner<R>> {
        Arc::new(NostrIdentityProvisioner {
            gateways: self.gateways.clone(),
            admins: self.admins.clone(),
            runner: self.runner.clone(),
            cli: self.cli.clone(),
            runtime: self.runtime.clone(),
            timed_fire_router: self.timed_fire_router.clone(),
            allow_store: self.allow_store.clone(),
            provisioner: self.provisioner.clone(),
            reviser: self.reviser.clone(),
        })
    }

    /// enabled な設定を DB から復元し、どれか一つでも現在設定を投影できなければ失敗する。
    pub async fn restore_from_db_checked(&self) -> anyhow::Result<()> {
        for cfg in self.runner.list_enabled_nostr_configs() {
            let config = crate::config_from_row(&cfg);
            self.start_agent_gateway(&cfg.agent_id, &cfg.secret_key, config)
                .await
                .map_err(|error| {
                    anyhow::anyhow!("failed to restore Nostr gateway {}: {error}", cfg.agent_id)
                })?;
        }
        Ok(())
    }

    /// Registry lifecycle compatibility wrapper. Production startup uses the checked variant.
    pub async fn restore_from_db(&self) {
        if let Err(error) = self.restore_from_db_checked().await {
            error!(error = %error, "Failed to restore Nostr gateways");
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
    /// （未稼働なら bootstrap 起動＝接続、稼働中なら停止→revision→V3 再起動）ので、
    /// `key_provisioning` と同じく `is_running` に関わらず
    /// 常に `Some` を返す。
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

#[cfg(test)]
#[path = "manager/tests/mod.rs"]
mod tests;
