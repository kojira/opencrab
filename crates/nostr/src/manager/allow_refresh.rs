use super::*;

/// #698: 元栓の許可集合を構築する。フォロイー（relay 由来 / `fetch_following`）＋ owner /
/// co_agent / trusted_users（DB 由来 / `nostr_gate_allow_keys`）を [`follow_key`] で正規化して
/// 1 つの [`AllowSources`] に合成する。
///
/// **取得の失敗はすべて `Err`**（呼び出し側が起動中止 or 前回値保持で fail-loud に扱う）:
/// フォローリスト（relay）取得の失敗も、DB 由来キーの取得失敗（lock poison / query Err）も、
/// どちらもここで `?` により `Err` になる。DB 側を `Ok(空)` に握り潰さないことで、owner/trusted が
/// DB エラーで無音でキャッシュから消えるのを防ぐ（「未登録＝空」と「DB 故障＝読めない」を区別）。
/// DB 由来のキーは**単一源**（判定は resolve_nostr_caller と同じ DB 表を材料にする）。正規化は
/// ここ 1 箇所に閉じる（author_key と同じ `follow_key`。書き手と読み手のキーずれ事故を防ぐ）。
pub(super) async fn build_allow_sources<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
) -> anyhow::Result<AllowSources> {
    // relay 由来（fail-loud）。
    let followees: HashSet<String> = cli
        .fetch_following(agent_id)
        .await?
        .iter()
        .map(|k| crate::pubkey::follow_key(k))
        .collect();
    // DB 由来（owner / co_agent / trusted_users）。DB 故障は `?` で伝播（Ok(空) に化けさせない）。
    let db = runner.nostr_gate_allow_keys(agent_id)?;
    let to_set = |v: &[String]| -> HashSet<String> {
        v.iter().map(|s| crate::pubkey::follow_key(s)).collect()
    };
    Ok(AllowSources {
        followees,
        owner: to_set(&db.owner),
        co_agents: to_set(&db.co_agents),
        trusted_users: to_set(&db.trusted_users),
    })
}

/// #698: 許可集合を 1 回引き直して `allow` セルへ反映する。
///
/// 成功なら差し替え、**失敗なら前回値を保持**して warn（全通しへは倒さない = fail-loud だが
/// 権威データは保つ）。fetch_following（relay）の失敗も nostr_gate_allow_keys（DB）の失敗も
/// [`build_allow_sources`] が `Err` にまとめるので、**どちらも同じ「前回値保持」に合流**する
/// （owner/trusted が DB エラーで無音で消えない）。
pub(super) async fn refresh_allow_once<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    allow: &AllowGate,
    store: &AllowSetStore,
) {
    match build_allow_sources(runner, cli, agent_id).await {
        Ok(next) => {
            let (f, o, c, t) = (
                next.followees.len(),
                next.owner.len(),
                next.co_agents.len(),
                next.trusted_users.len(),
            );
            *allow.write().unwrap() = next.clone();
            store.replace_allow(agent_id, next);
            debug!(agent_id = %agent_id, followees = f, owner = o, co_agents = c, trusted_users = t, "nostr: 元栓の許可集合を更新（#698）");
        }
        Err(e) => {
            warn!(agent_id = %agent_id, error = %format!("{e:#}"), "nostr: 許可集合の更新に失敗。前回値を保持（relay/DB いずれの失敗も全通しへは倒さない / #698）");
        }
    }
}

/// #698: 許可集合を引き直して `allow` セルを差し替える detached task を回す。
///
/// `fetch_following` は relay 往復で最大 nostaro timeout（60s）ぶんかかるので、受信ループを
/// 塞がないように spawn する（更新中も新着は既存 `allow` で判定され続ける）。反映と失敗時の
/// 前回値保持は [`refresh_allow_once`] が担う。timeout があるので tick ごとにハングが堆積しない
/// （間隔 300s ≫ 60s）。
pub(super) fn spawn_allow_refresh<R: NostrAgentRunner + Clone>(
    runner: R,
    cli: NostaroCli,
    agent_id: String,
    allow: AllowGate,
    store: AllowSetStore,
) {
    tokio::spawn(async move {
        refresh_allow_once(&runner, &cli, &agent_id, &allow, &store).await;
    });
}

/// 1 回分の watch 購読（プロセス寿命ぶん）。
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_watch_once<R: NostrAgentRunner + Clone>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    config: &NostrConfig,
    self_pubkey: &SelfPubkey,
    // #698 元栓: 許可集合セルと、捨てた件数の揮発カウンタ。
    allow: &AllowGate,
    store: &AllowSetStore,
    dropped: &Arc<AtomicU64>,
    admin: &Arc<dyn NostrIdentityAdmin>,
    runtime: &Arc<NostrSessionRuntime>,
    permits: &Arc<Semaphore>,
    queues: &Arc<SessionQueues>,
    seen: &mut SeenEvents,
    shadows: bool,
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

    // #698 許可集合の追従: この subscription の寿命中、定期的に許可集合（フォロイー ∪ owner ∪
    // co_agent ∪ trusted_users）を引き直す。起動時に構築済みなので初回 tick は INTERVAL 後
    // （`interval_at`）。tick は event 処理より優先しない（`biased` で受信を先に捌く / #178 の
    // 「受信ループを塞がない」に揃える。洪水中は更新が後回しになるだけで、既存 allow で元栓は
    // 効き続ける）。
    let mut refresh = tokio::time::interval_at(
        tokio::time::Instant::now() + ALLOW_REFRESH_INTERVAL,
        ALLOW_REFRESH_INTERVAL,
    );
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    info!(agent_id, relays = ?config.effective_relays(), "nostr watch subscribed");
    loop {
        let line = tokio::select! {
            biased;
            line = lines.next_line() => line?,
            _ = refresh.tick() => {
                spawn_allow_refresh(runner.clone(), cli.clone(), agent_id.to_string(), allow.clone(), store.clone());
                // 揮発カウンタの累計を節目で 1 行だけ可視化（毎行は出さない / #698）。
                let total = dropped.load(AtomicOrdering::Relaxed);
                if total > 0 {
                    debug!(agent_id, dropped = total, "nostr: 未許可作者の元栓で捨てた累計（#698・揮発）");
                }
                continue;
            }
        };
        let Some(line) = line else { break };
        if shadows {
            crate::shadow::compare_parse(&line);
        }
        let Some(event) = parse_watch_line(&line) else {
            continue;
        };
        if shadows {
            crate::shadow::compare_classify(
                &event,
                &self_pubkey.read().unwrap(),
                config.watches_beyond_self_mentions(),
                None,
            );
        }
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
        let self_pk = self_pubkey.read().unwrap().clone();
        handle_event(
            runner, cli, agent_id, &self_pk, allow, dropped, admin, runtime, permits, queues, event,
        )
        .await;
    }
    // stdout EOF → プロセス終了を回収。
    let _ = child.wait().await;
    Ok(())
}
