use super::*;

/// V3 の identity 切替: 鍵更新のあと gateway を停止→revision→再起動する。
struct V3IdentityRestart<R: NostrAgentRunner> {
    gateways: GatewayMap,
    admins: AdminMap,
    runner: R,
    cli: NostaroCli,
    runtime: Arc<NostrSessionRuntime>,
    timed_fire_router: Arc<opencrab_actions::TimedFireRouter>,
    allow_store: AllowSetStore,
    provisioner: Option<NostrProvisionFn>,
    reviser: Option<NostrReviseFn>,
}

impl<R: NostrAgentRunner> V3IdentityRestart<R> {
    async fn after_switch(
        &self,
        agent_id: &str,
        secret_key: &str,
        self_pubkey: &str,
    ) -> anyhow::Result<()> {
        stop_gateway(
            &self.gateways,
            &self.admins,
            &self.timed_fire_router,
            &self.allow_store,
            agent_id,
        );
        let row = self
            .runner
            .get_nostr_config(agent_id)
            .ok_or_else(|| anyhow::anyhow!("Nostr 設定が無いので revision を上げられない"))?;
        let config = crate::config_from_row(&row);
        let watches = self
            .runner
            .list_session_watches_for_agent(agent_id)
            .map_err(|e| anyhow::anyhow!("session_watches を読めない: {e:#}"))?;
        let Some(revise) = self.reviser.as_ref() else {
            anyhow::bail!("nostr_ingress=v3 なのに reviser が無い");
        };
        revise(agent_id, self_pubkey, &config, &watches)?;
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
}

/// identity 切替の実体。runner（DB）+ cli + self_pubkey セルを capture し、
/// 生成鍵を本鍵に採用した後、V3 gateway を停止→revision→再起動する。
struct LoopIdentityAdmin<R: NostrAgentRunner> {
    runner: R,
    cli: NostaroCli,
    self_pubkey: SelfPubkey,
    v3_restart: Option<Arc<V3IdentityRestart<R>>>,
}

#[async_trait::async_trait]
impl<R: NostrAgentRunner> NostrIdentityAdmin for LoopIdentityAdmin<R> {
    async fn adopt_generated_identity(&self, agent_id: &str, npub: &str) -> anyhow::Result<String> {
        // 生成鍵（自分が作ったもの）の nsec をサーバ内で復号して読む。存在チェックで
        // 「自分が生成した鍵のみ採用可」を担保。秘密鍵は外へ出さない。
        let nsec = self.cli.read_generated_key(agent_id, npub)?;
        // 既存設定（relays/filter を継承）。未設定なら採用しない。
        let row = self.runner.get_nostr_config(agent_id).ok_or_else(|| {
            anyhow::anyhow!("Nostr 未設定です。先に Nostr を設定してから本鍵を切り替えてください")
        })?;
        let config = crate::config_from_row(&row);
        let relays = config.effective_relays();
        // ロールバック用に旧 pubkey を控える（#620: 本鍵はもう config に無いので、config
        // ロールバックは不要になった。巻き戻すのは自己スキップセルだけ）。
        let old_pubkey = self.self_pubkey.read().unwrap().clone();

        // 1) config.toml を鍵行なしで再生成（relays 継承）。鍵は実行時に env で注入する。
        NostaroCli::materialize_config(agent_id, &relays, None)?;

        // 2) 新 pubkey を**生成鍵経由**で取得する（#620）。本鍵プロバイダは DB を読むため、
        //    DB 更新前の `pubkey()` は旧鍵の pubkey を返してしまう。`pubkey_from` は生成鍵を
        //    env で注入して nostaro に引かせるので、DB 更新の**前**に新鍵の検証と新 pubkey の
        //    取得を同時に行える。**fail-closed**: 取れないと自己スキップが旧 pubkey のままに
        //    なり自己返信ループ＋LLM 課金になるので、DB を触らず中止する（config は鍵無しなので
        //    巻き戻し不要）。
        let new_pubkey = match self.cli.pubkey_from(agent_id, npub).await {
            Ok(pk) if !pk.trim().is_empty() => pk.trim().to_string(),
            _ => {
                anyhow::bail!(
                    "新しい鍵の pubkey を取得できませんでした。自己返信ループ防止のため切替を中止しました"
                );
            }
        };

        // 3) 自己スキップ用セルを更新（以後の自己スキップが新 identity 追従）。
        *self.self_pubkey.write().unwrap() = new_pubkey.clone();

        // 4) DB を最後に更新（runner が暗号化して保存する）。失敗したらセルを旧状態へ巻き戻す
        //    （DB=旧 / セル=新 の不整合を残さない）。
        if let Err(e) = self.runner.set_nostr_secret_key(agent_id, &nsec) {
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
        if let Some(restart) = &self.v3_restart {
            restart.after_switch(agent_id, &nsec, &new_pubkey).await?;
        }
        Ok(npub.to_string())
    }
}

/// gateway を停止する（handle abort + 採用 admin 除去）。稼働していなければ何もしない。
pub(super) fn stop_gateway(
    gateways: &GatewayMap,
    admins: &AdminMap,
    timed_fire_router: &Arc<opencrab_actions::TimedFireRouter>,
    allow_store: &AllowSetStore,
    agent_id: &str,
) {
    let handle = gateways.write().unwrap().remove(agent_id);
    // 採用 admin も一緒に外す（gateways と生死を揃える＝停止後に稼働中と誤判定させない）。
    admins.write().unwrap().remove(agent_id);
    allow_store.remove(agent_id);
    // #588 TimedFire: 死んだループへ発火が消えないよう受け口を解除する。Nostr は共有ゲートウェイが
    // 無い（per-agent のみ）ので、解除しないと停止後の時刻発火が宛先なく捨てられる（#603: 必須）。
    timed_fire_router.unregister_per_agent(opencrab_actions::gateway_kinds::NOSTR, agent_id);
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
pub(super) async fn spawn_agent_gateway<R: NostrAgentRunner>(
    gateways: &GatewayMap,
    admins: &AdminMap,
    runner: &R,
    cli: &NostaroCli,
    runtime: &Arc<NostrSessionRuntime>,
    agent_id: &str,
    secret_key: &str,
    config: NostrConfig,
    timed_fire_router: Arc<opencrab_actions::TimedFireRouter>,
    allow_store: AllowSetStore,
    provisioner: Option<NostrProvisionFn>,
    reviser: Option<NostrReviseFn>,
) -> anyhow::Result<()> {
    // 資格情報のガード（#191 段階2 PR3）。DB の secret_key（#620 以降は暗号文 `enc:v1:…`）が
    // 空 / 空白だけなら鍵未設定なので、materialize（config 書き出し）や `pubkey` 取得より
    // **手前**で拒否する。暗号文は必ず非空（空鍵は暗号化しない）なので、この非空検査で足りる。
    // 実際の鍵は本鍵プロバイダが DB から復号して env で注入する。
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

    stop_gateway(gateways, admins, &timed_fire_router, &allow_store, agent_id);

    // #620: config は**鍵行なし**（relays のみ）。鍵は base_command が env で注入する。
    NostaroCli::materialize_config(agent_id, &config.effective_relays(), None)?;

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

    // #698 元栓: 許可集合（フォロイー ∪ owner ∪ co_agent ∪ trusted_users）を**起動時に構築**する。
    // 未信頼作者のイベントを着火も記録もさせず捨てるゲート（[`handle_event`]）の権威データ。
    // フォローリスト（relay）取得に失敗したら **起動しない**（fail-closed かつ fail-loud）:
    // ここで空集合や全通しへ黙って倒すと、オーナー裁定（未信頼作者でターンを起こさせない）の
    // 元栓が外れたまま無防備に走る。`self_pubkey` 取得失敗で起動中止するのと同じ流儀。0 フォロー
    // （空集合）は正当な成功で、その場合ゲートは owner / co_agent / trusted_users のみ通す。
    let allow_sources = build_allow_sources(runner, cli, agent_id)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "許可集合（フォローリスト kind:3 ほか）を構築できませんでした: {e:#}。#698 の元栓は\
                 許可集合を権威データにするため、取得できないまま全通しへ倒さず起動を中止します"
            )
        })?;
    info!(
        agent_id,
        followees = allow_sources.followees.len(),
        owner = allow_sources.owner.len(),
        co_agents = allow_sources.co_agents.len(),
        trusted_users = allow_sources.trusted_users.len(),
        "nostr: 元栓の許可集合を構築（フォロイー ∪ owner ∪ co_agent ∪ trusted_users / #698）"
    );
    allow_store.replace_allow(agent_id, allow_sources.clone());
    allow_store.set_self_pubkey(agent_id, self_pubkey.clone());
    let allow: AllowGate = Arc::new(RwLock::new(allow_sources));

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

    let watches = runner
        .list_session_watches_for_agent(agent_id)
        .map_err(|e| anyhow::anyhow!("session_watches を読めない: {e:#}"))?;
    let relays = config.effective_relays();
    for w in &watches {
        if !w.session_id.starts_with(NOSTR_SESSION_PREFIX) {
            anyhow::bail!(
                "session_watches.id={} の session_id が nostr- 系ではない（Q-B）",
                w.id
            );
        }
        watch_subscribe_config(w, relays.clone())?;
    }
    let Some(provision) = provisioner.as_ref() else {
        anyhow::bail!("Nostr V3 binding provisioner が無い");
    };
    provision(agent_id, &self_pubkey, &config, &watches)?;

    let runner_c = runner.clone();
    let cli_c = cli.clone();
    let agent = agent_id.to_string();
    // self_pubkey は共有セル。identity 切替は停止→revision→V3 再起動。
    let self_pubkey_cell = Arc::new(RwLock::new(self_pubkey));
    let v3_restart = Some(Arc::new(V3IdentityRestart {
        gateways: gateways.clone(),
        admins: admins.clone(),
        runner: runner_c.clone(),
        cli: cli_c.clone(),
        runtime: runtime.clone(),
        timed_fire_router: timed_fire_router.clone(),
        allow_store: allow_store.clone(),
        provisioner: provisioner.clone(),
        reviser: reviser.clone(),
    }));
    let admin: Arc<dyn NostrIdentityAdmin> = Arc::new(LoopIdentityAdmin {
        runner: runner_c.clone(),
        cli: cli_c.clone(),
        self_pubkey: self_pubkey_cell.clone(),
        v3_restart,
    });
    // 採用 admin を登録簿へ（handle と同時に入れる＝稼働中の判定と生死が揃う）。
    admins
        .write()
        .unwrap()
        .insert(agent_id.to_string(), admin.clone());
    let handle = tokio::spawn(async move {
        run_v3_core_keep_alive(runner_c, cli_c, agent, allow, allow_store).await;
    });

    gateways
        .write()
        .unwrap()
        .insert(agent_id.to_string(), handle);
    info!(agent_id, "Per-agent Nostr gateway started");
    Ok(())
}
