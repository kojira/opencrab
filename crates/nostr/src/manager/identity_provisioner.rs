use super::*;

/// 生成鍵の採用（identity 切替）capability の実体（#264）。
///
/// マネージャと**同じ登録簿**（`gateways` / `admins` の Arc）を共有する。判定は 2 モード:
/// - **稼働中 + legacy/shadow** → per-agent admin で in-place ホットスワップ。
/// - **稼働中 + v3** → 鍵更新のあと停止→revision→再起動。
/// - **未稼働（自己ブートストラップ）** → `agent_nostr_config` に鍵・リレー・**空フィルタ**
///   （＝nostaro の mention-only 既定に委ねて自分宛のみ / #271）を enabled=false で書き、
///   [`spawn_agent_gateway`] で起動＝接続、
///   成功後に enabled=true。これで「未設定エージェントが `nostr_generate_key`→
///   `nostr_switch_identity` を呼ぶだけで自力で載る」が成立する。
pub struct NostrIdentityProvisioner<R: NostrAgentRunner> {
    pub(super) gateways: GatewayMap,
    pub(super) admins: AdminMap,
    pub(super) runner: R,
    pub(super) cli: NostaroCli,
    pub(super) runtime: Arc<NostrSessionRuntime>,
    /// #588 TimedFire / #603: 採用時 bootstrap 起動でも時刻発火の受け口を登録する（本体と同じ
    /// 登録簿・**必須**）。
    pub(super) timed_fire_router: Arc<opencrab_actions::TimedFireRouter>,
    pub(super) ingress: NostrIngress,
    pub(super) allow_store: AllowSetStore,
    pub(super) provisioner: Option<NostrProvisionFn>,
    pub(super) instance_provisioner: Option<NostrInstanceFn>,
    pub(super) reviser: Option<NostrReviseFn>,
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
        // 稼働中: legacy はホットスワップ、v3 は admin 内で停止→revision→再起動。
        let running_admin = self.admins.read().unwrap().get(agent_id).cloned();
        if let Some(admin) = running_admin {
            return admin.adopt_generated_identity(agent_id, npub).await;
        }

        // 未稼働＝自己ブートストラップ。生成鍵（自分のもの）の nsec を復号して読む。存在
        // チェックで「自分が生成した鍵のみ採用可」を担保。秘密鍵は外へ出さない・返さない。
        let nsec = self.cli.read_generated_key(agent_id, npub)?;
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
            self.timed_fire_router.clone(),
            self.ingress,
            self.allow_store.clone(),
            self.provisioner.clone(),
            self.instance_provisioner.clone(),
            self.reviser.clone(),
        )
        .await?;

        // 3) 起動成功後に enabled=true（次回のプロセス再起動で restore_from_db が復元する）。
        self.runner.set_nostr_enabled(agent_id, true)?;
        Ok(npub.to_string())
    }
}
