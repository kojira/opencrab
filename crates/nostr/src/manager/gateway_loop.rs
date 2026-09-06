use super::*;

#[allow(clippy::too_many_arguments)]
async fn run_nostr_loop<R: NostrAgentRunner + Clone>(
    runner: R,
    cli: NostaroCli,
    agent_id: String,
    config: NostrConfig,
    self_pubkey: SelfPubkey,
    // #698 元栓: 許可集合（フォロイー ∪ owner ∪ co_agent ∪ trusted_users）。ゲートが読み、
    // watch ループが定期更新でこのセルを差し替える。
    allow: AllowGate,
    store: AllowSetStore,
    admin: Arc<dyn NostrIdentityAdmin>,
    runtime: Arc<NostrSessionRuntime>,
    // #588 TimedFire / #603: 時刻発火の受け口を登録する登録簿（**必須**）。
    timed_fire_router: Arc<opencrab_actions::TimedFireRouter>,
    shadows: bool,
) {
    let mut seen = SeenEvents::new(512);
    // #698: 元栓で捨てた件数の**揮発カウンタ**（プロセス寿命ぶん・永続しない）。毎行ログは
    // フラッド時に費用になるので残さず、ここに数えるだけ。運用可視化は更新経路が節目で 1 行出す。
    let dropped = Arc::new(AtomicU64::new(0));
    // 応答生成の流量制限。watch 再購読を跨いで同じ permit プールを使う。
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_RESPONSES));
    // per-session の FIFO キュー。再購読を跨いで同じものを使う（購読が張り直されても
    // 処理待ちの順序と consumer を落とさない）。
    let queues = Arc::new(SessionQueues::new(SESSION_QUEUE_CAPACITY));
    // #588 TimedFire: scheduler の時刻発火をこのループのキューへ流す受け口を登録する（#603: 必須）。
    timed_fire_router.register_per_agent(
        opencrab_actions::gateway_kinds::NOSTR,
        &agent_id,
        Arc::new(NostrTimedFireSink {
            runner: runner.clone(),
            cli: cli.clone(),
            runtime: runtime.clone(),
            admin: admin.clone(),
            queues: queues.clone(),
            permits: permits.clone(),
        }),
    );
    // #603: 登録が起きたことを起動時に 1 行残す（Discord と対称・運用で可視化）。
    info!(
        agent_id = %agent_id,
        transport = "nostr",
        "timed-fire: 受け口を登録（per-agent Nostr loop）"
    );
    loop {
        match run_watch_once(
            &runner,
            &cli,
            &agent_id,
            &config,
            &self_pubkey,
            &allow,
            &store,
            &dropped,
            &admin,
            &runtime,
            &permits,
            &queues,
            &mut seen,
            shadows,
        )
        .await
        {
            Ok(()) => warn!(agent_id, "nostr watch exited; restarting after backoff"),
            Err(e) => error!(agent_id, error = %e, "nostr watch error; restarting after backoff"),
        }
        tokio::time::sleep(WATCH_RESTART_DELAY).await;
    }
}

/// 現行 `nostr-{agent}` ループと `session_watches` 購読を同じ gateway handle の下で走らせる。
///
/// 既定セッションに watch 行があるときは現行ループを起動しない（そのセッションは新機構）。
/// watch が 0 なら現行ループだけ（挙動不変）。
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_agent_inbound_loops<R: NostrAgentRunner + Clone + 'static>(
    runner: R,
    cli: NostaroCli,
    agent_id: String,
    config: NostrConfig,
    self_pubkey: SelfPubkey,
    allow: AllowGate,
    store: AllowSetStore,
    admin: Arc<dyn NostrIdentityAdmin>,
    runtime: Arc<NostrSessionRuntime>,
    timed_fire_router: Arc<opencrab_actions::TimedFireRouter>,
    watches: Vec<opencrab_db::queries::SessionWatchRow>,
    skip_default_loop: bool,
    shadows: bool,
) {
    let mut set = tokio::task::JoinSet::new();
    if !skip_default_loop {
        let runner_c = runner.clone();
        let cli_c = cli.clone();
        let agent = agent_id.clone();
        let config_c = config.clone();
        let self_pk = self_pubkey.clone();
        let allow_c = allow.clone();
        let store_c = store.clone();
        let admin_c = admin.clone();
        let runtime_c = runtime.clone();
        let router = timed_fire_router.clone();
        set.spawn(async move {
            run_nostr_loop(
                runner_c, cli_c, agent, config_c, self_pk, allow_c, store_c, admin_c, runtime_c,
                router, shadows,
            )
            .await;
        });
    }
    for watch in watches {
        let runner_c = runner.clone();
        let cli_c = cli.clone();
        let agent = agent_id.clone();
        let relays = config.effective_relays();
        let self_pk = self_pubkey.clone();
        let allow_c = allow.clone();
        let store_c = store.clone();
        let admin_c = admin.clone();
        let runtime_c = runtime.clone();
        set.spawn(async move {
            run_session_watch_loop(
                runner_c, cli_c, agent, relays, watch, self_pk, allow_c, store_c, admin_c,
                runtime_c, shadows,
            )
            .await;
        });
    }
    while set.join_next().await.is_some() {}
}

/// V3: 旧 in-process 購読は起動しない。TimedFire 受け口と元栓 300 秒更新だけ残す。
///
/// Binding PUT / said は gateway 側。core 側は時刻発火と allow-set の権威を維持する。
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_v3_core_keep_alive<R: NostrAgentRunner + Clone>(
    runner: R,
    cli: NostaroCli,
    agent_id: String,
    _self_pubkey: SelfPubkey,
    allow: AllowGate,
    store: AllowSetStore,
    admin: Arc<dyn NostrIdentityAdmin>,
    runtime: Arc<NostrSessionRuntime>,
    timed_fire_router: Arc<opencrab_actions::TimedFireRouter>,
) {
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_RESPONSES));
    let queues = Arc::new(SessionQueues::new(SESSION_QUEUE_CAPACITY));
    timed_fire_router.register_per_agent(
        opencrab_actions::gateway_kinds::NOSTR,
        &agent_id,
        Arc::new(NostrTimedFireSink {
            runner: runner.clone(),
            cli: cli.clone(),
            runtime: runtime.clone(),
            admin: admin.clone(),
            queues: queues.clone(),
            permits: permits.clone(),
        }),
    );
    info!(
        agent_id = %agent_id,
        transport = "nostr",
        "timed-fire: 受け口を登録（V3 core keep-alive）"
    );
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + ALLOW_REFRESH_INTERVAL,
        ALLOW_REFRESH_INTERVAL,
    );
    loop {
        ticker.tick().await;
        refresh_allow_once(&runner, &cli, &agent_id, &allow, &store).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session_watch_loop<R: NostrAgentRunner + Clone>(
    runner: R,
    cli: NostaroCli,
    agent_id: String,
    relays: Vec<String>,
    watch: opencrab_db::queries::SessionWatchRow,
    self_pubkey: SelfPubkey,
    allow: AllowGate,
    store: AllowSetStore,
    admin: Arc<dyn NostrIdentityAdmin>,
    runtime: Arc<NostrSessionRuntime>,
    shadows: bool,
) {
    let dropped = Arc::new(AtomicU64::new(0));
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_RESPONSES));
    let queues = Arc::new(SessionQueues::new(SESSION_QUEUE_CAPACITY));
    let bundle = Arc::new(Mutex::new(TimelineBundle::default()));
    let privilege = opencrab_actions::PrivilegeFire::new({
        let runner = runner.clone();
        let cli = cli.clone();
        let agent = agent_id.clone();
        let session_id = watch.session_id.clone();
        let allow = allow.clone();
        let admin = admin.clone();
        let runtime = runtime.clone();
        let permits = permits.clone();
        let queues = queues.clone();
        move |held: Vec<(NostrEvent, opencrab_actions::CallerIdentity)>| {
            let runner = runner.clone();
            let cli = cli.clone();
            let agent = agent.clone();
            let session_id = session_id.clone();
            let allow = allow.clone();
            let admin = admin.clone();
            let runtime = runtime.clone();
            let permits = permits.clone();
            let queues = queues.clone();
            async move {
                fire_privilege_held(
                    &runner,
                    &cli,
                    &agent,
                    &session_id,
                    &allow,
                    &admin,
                    &runtime,
                    &permits,
                    &queues,
                    held,
                );
            }
        }
    });
    loop {
        match run_session_watch_once(
            &runner,
            &cli,
            &agent_id,
            &relays,
            &watch,
            &self_pubkey,
            &allow,
            &store,
            &dropped,
            &admin,
            &runtime,
            &permits,
            &queues,
            &bundle,
            &privilege,
            shadows,
        )
        .await
        {
            Ok(()) => warn!(
                agent_id,
                watch_id = watch.id,
                "session watch exited; restarting after backoff"
            ),
            Err(e) => error!(
                agent_id,
                watch_id = watch.id,
                error = %e,
                "session watch error; restarting after backoff"
            ),
        }
        tokio::time::sleep(WATCH_RESTART_DELAY).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session_watch_once<R: NostrAgentRunner + Clone>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    relays: &[String],
    watch: &opencrab_db::queries::SessionWatchRow,
    self_pubkey: &SelfPubkey,
    allow: &AllowGate,
    store: &AllowSetStore,
    dropped: &Arc<AtomicU64>,
    admin: &Arc<dyn NostrIdentityAdmin>,
    runtime: &Arc<NostrSessionRuntime>,
    permits: &Arc<Semaphore>,
    queues: &Arc<SessionQueues>,
    bundle: &Arc<Mutex<TimelineBundle>>,
    privilege: &opencrab_actions::PrivilegeFire<NostrEvent>,
    shadows: bool,
) -> anyhow::Result<()> {
    let config = watch_subscribe_config(watch, relays.to_vec())?;
    let interval = Duration::from_secs(watch.interval_secs as u64);
    let mut cmd = cli.build_watch_command(agent_id, &config)?;
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn `nostaro watch` for session_watches: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("nostaro watch produced no stdout handle"))?;
    let mut lines = BufReader::new(stdout).lines();
    let mut seen = SeenEvents::new(512);
    let mut refresh = tokio::time::interval_at(
        tokio::time::Instant::now() + ALLOW_REFRESH_INTERVAL,
        ALLOW_REFRESH_INTERVAL,
    );
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut flush = tokio::time::interval(interval);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    info!(
        agent_id,
        session_id = %watch.session_id,
        watch_id = watch.id,
        interval_secs = watch.interval_secs,
        "session watch subscribed"
    );
    loop {
        tokio::select! {
            biased;
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                if shadows {
                    crate::shadow::compare_parse(&line);
                }
                let Some(event) = parse_watch_line(&line) else { continue };
                if shadows {
                    let self_pk = self_pubkey.read().unwrap().clone();
                    crate::shadow::compare_classify(
                        &event,
                        &self_pk,
                        config.watches_beyond_self_mentions(),
                        Some(watch.id),
                    );
                }
                if *self_pubkey.read().unwrap() == event.pubkey {
                    continue;
                }
                if !seen.check_and_insert(&event.id) {
                    continue;
                }
                handle_watch_event(
                    runner, cli, agent_id, watch, &config, self_pubkey, allow, dropped,
                    admin, runtime, permits, queues, bundle, privilege, event,
                )
                .await;
            }
            _ = flush.tick() => {
                flush_timeline_bundle(
                    runner, cli, agent_id, watch, allow, admin, runtime, permits, queues,
                    bundle,
                )
                .await;
            }
            _ = refresh.tick() => {
                spawn_allow_refresh(runner.clone(), cli.clone(), agent_id.to_string(), allow.clone(), store.clone());
            }
        }
    }
    let _ = child.wait().await;
    Ok(())
}
