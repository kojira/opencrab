use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_watch_event<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    watch: &opencrab_db::queries::SessionWatchRow,
    config: &NostrConfig,
    self_pubkey: &SelfPubkey,
    allow: &AllowGate,
    dropped: &Arc<AtomicU64>,
    admin: &Arc<dyn NostrIdentityAdmin>,
    runtime: &Arc<NostrSessionRuntime>,
    permits: &Arc<Semaphore>,
    queues: &Arc<SessionQueues>,
    bundle: &Arc<Mutex<TimelineBundle>>,
    privilege: &opencrab_actions::PrivilegeFire<NostrEvent>,
    event: NostrEvent,
) {
    let self_pk = self_pubkey.read().unwrap().clone();
    match classify_watch_event(&event, &self_pk, config.watches_beyond_self_mentions()) {
        WatchForward::Discard => {}
        WatchForward::Bundle { .. } => {
            bundle.lock().unwrap().push(event);
        }
        WatchForward::Immediate { label } => {
            if let Some(reason) = pre_record_drop(&event, &self_pk, &allow.read().unwrap()) {
                if matches!(reason, DropReason::AllowSet) {
                    dropped.fetch_add(1, AtomicOrdering::Relaxed);
                }
                return;
            }
            let policy = match runner.get_session_policy_json(&watch.session_id) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    error!(session_id = %watch.session_id, "policy_json を読めない（session が無い）");
                    return;
                }
                Err(e) => {
                    error!(session_id = %watch.session_id, error = %e, "policy_json 読み失敗");
                    return;
                }
            };
            let sources = allow.read().unwrap();
            let allow_sets = sources.as_watch_allow();
            let hold_event = event.clone();
            let mut held = false;
            let mut admitted = false;
            let mut run_caller = None;
            let owner_id = sources.owner.iter().next().cloned().unwrap_or_default();
            let resolve =
                |sender: &str, _: &[String], _: &str| runner.resolve_nostr_caller(agent_id, sender);
            let dm_any = |sender: &str, _: &[String], owner: &str| sender == owner;
            let dm_one = |sender: &str, _: &str, owner: &str| sender == owner;
            let sid = watch.session_id.clone();
            let wl = move |channel: &str, aid: &str| {
                channel == sid || channel == crate::session::nostr_session_id(aid)
            };
            let lookups = opencrab_actions::InboundLookups {
                resolve_caller: &resolve,
                dm_allowed_any: &dm_any,
                dm_allowed: &dm_one,
                channel_whitelisted: &wl,
            };
            let result = accept_nostr_inbound(
                &event,
                agent_id,
                &watch.session_id,
                &owner_id,
                label,
                &lookups,
                Some(opencrab_actions::WatchAccept {
                    policy_json: &policy,
                    interval_secs: watch.interval_secs as u64,
                    allow: allow_sets,
                    owner: &sources.owner,
                    followees: &sources.followees,
                    privilege: Some(privilege),
                }),
                |_| {
                    held = true;
                    hold_event.clone()
                },
                |_, _| admitted = true,
                |_, adm, _| run_caller = Some(adm.caller.clone()),
            );
            drop(sources);
            if let Err(e) = result {
                error!(error = %e, "watch inbound 失敗");
                return;
            }
            if held {
                return;
            }
            if !admitted {
                dropped.fetch_add(1, AtomicOrdering::Relaxed);
                return;
            }
            if !prepare_and_maybe_relay(runner, agent_id, &watch.session_id, &event) {
                return;
            }
            if let Some(caller) = run_caller {
                enqueue_watch_turn(
                    runner,
                    cli,
                    agent_id,
                    &watch.session_id,
                    admin,
                    runtime,
                    permits,
                    queues,
                    event,
                    label,
                    caller,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn flush_timeline_bundle<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    watch: &opencrab_db::queries::SessionWatchRow,
    allow: &AllowGate,
    admin: &Arc<dyn NostrIdentityAdmin>,
    runtime: &Arc<NostrSessionRuntime>,
    permits: &Arc<Semaphore>,
    queues: &Arc<SessionQueues>,
    bundle: &Arc<Mutex<TimelineBundle>>,
) {
    let bundled = bundle.lock().unwrap().take();
    if bundled.is_empty() {
        return;
    }
    flush_event_group(
        runner,
        cli,
        agent_id,
        &watch.session_id,
        allow,
        admin,
        runtime,
        permits,
        queues,
        bundled,
        true,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn fire_privilege_held<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    session_id: &str,
    allow: &AllowGate,
    admin: &Arc<dyn NostrIdentityAdmin>,
    runtime: &Arc<NostrSessionRuntime>,
    permits: &Arc<Semaphore>,
    queues: &Arc<SessionQueues>,
    held: Vec<(NostrEvent, opencrab_actions::CallerIdentity)>,
) {
    if held.is_empty() {
        return;
    }
    let sources = allow.read().unwrap();
    let mut prepared = vec![false; held.len()];
    for (i, (event, _)) in held.iter().enumerate() {
        if !sources.is_allowed(&crate::pubkey::follow_key(&event.pubkey)) {
            continue;
        }
        prepared[i] = prepare_and_maybe_relay(runner, agent_id, session_id, event);
    }
    drop(sources);
    let last = held.len() - 1;
    if !prepared[last] {
        return;
    }
    let (event, caller) = held[last].clone();
    let label = crate::watch::watch_kind_label(&event);
    enqueue_watch_turn(
        runner, cli, agent_id, session_id, admin, runtime, permits, queues, event, label, caller,
    );
}

#[allow(clippy::too_many_arguments)]
fn flush_event_group<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    session_id: &str,
    allow: &AllowGate,
    admin: &Arc<dyn NostrIdentityAdmin>,
    runtime: &Arc<NostrSessionRuntime>,
    permits: &Arc<Semaphore>,
    queues: &Arc<SessionQueues>,
    events: Vec<NostrEvent>,
    timeline: bool,
) {
    if events.is_empty() {
        return;
    }
    let sources = allow.read().unwrap();
    let allow_sets = sources.as_watch_allow();
    let labels: Vec<&str> = events
        .iter()
        .map(|e| {
            if timeline {
                "タイムライン"
            } else {
                crate::watch::watch_kind_label(e)
            }
        })
        .collect();
    let empty_policy = "{}";
    let empty_set = std::collections::HashSet::new();
    let mut prepared: Vec<bool> = vec![false; events.len()];
    let mut run_at: Option<(usize, opencrab_actions::CallerIdentity)> = None;
    let result = accept_watch_events(
        runner,
        agent_id,
        session_id,
        &events,
        &labels,
        Some(opencrab_actions::WatchAccept {
            policy_json: empty_policy,
            interval_secs: 1,
            allow: allow_sets,
            owner: &empty_set,
            followees: &empty_set,
            privilege: None,
        }),
        |_| unreachable!("watch flush は権限デバウンスしない"),
        |i, _| {
            prepared[i] = prepare_and_maybe_relay(runner, agent_id, session_id, &events[i]);
        },
        |i, adm, _| run_at = Some((i, adm.caller.clone())),
    );
    drop(sources);
    if let Err(e) = result {
        error!(error = %e, "watch flush inbound 失敗");
        return;
    }
    let Some((i, caller)) = run_at else {
        return;
    };
    if !prepared[i] {
        return;
    }
    let kept: Vec<NostrEvent> = events
        .iter()
        .enumerate()
        .filter(|(j, _)| prepared[*j])
        .map(|(_, e)| e.clone())
        .collect();
    let last = events[i].clone();
    let label = labels[i];
    let suffix = if timeline {
        watch_bundle_prompt_suffix(&kept)
    } else {
        watch_prompt_suffix(&last, label)
    };
    enqueue_watch_turn_with_suffix(
        runner, cli, agent_id, session_id, admin, runtime, permits, queues, last, caller, suffix,
    );
}

fn prepare_and_maybe_relay<R: NostrAgentRunner>(
    runner: &R,
    agent_id: &str,
    session_id: &str,
    event: &NostrEvent,
) -> bool {
    let recorded = recorded_watch_text(runner, agent_id, session_id, event);
    let ok = prepare_watch_inbound(runner, session_id, agent_id, event, &recorded);
    if !ok {
        tracing::error!(
            session_id,
            agent_id,
            "failed to persist a watch inbound; skip turn"
        );
        return false;
    }
    if let Some(target) = runner.resolve_nostr_relay_target(agent_id) {
        let relay_text = format!(
            "[Nostr / {kind}] {author}\n{body}",
            kind = event.inbound_kind_label(),
            author = event.author_label(),
            body = event.inbound_text(),
        );
        runner.relay_inbound_notification(&target, relay_text);
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn enqueue_watch_turn<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    session_id: &str,
    admin: &Arc<dyn NostrIdentityAdmin>,
    runtime: &Arc<NostrSessionRuntime>,
    permits: &Arc<Semaphore>,
    queues: &Arc<SessionQueues>,
    event: NostrEvent,
    label: &str,
    caller: opencrab_actions::CallerIdentity,
) {
    let suffix = watch_prompt_suffix(&event, label);
    enqueue_watch_turn_with_suffix(
        runner, cli, agent_id, session_id, admin, runtime, permits, queues, event, caller, suffix,
    );
}

#[allow(clippy::too_many_arguments)]
fn enqueue_watch_turn_with_suffix<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    session_id: &str,
    admin: &Arc<dyn NostrIdentityAdmin>,
    runtime: &Arc<NostrSessionRuntime>,
    permits: &Arc<Semaphore>,
    queues: &Arc<SessionQueues>,
    event: NostrEvent,
    caller: opencrab_actions::CallerIdentity,
    prompt_suffix: String,
) {
    let responder_runner = runner.clone();
    let cli = cli.clone();
    let admin = admin.clone();
    let runtime = runtime.clone();
    let agent = agent_id.to_string();
    let sid = session_id.to_string();
    let reply_target = event.reply_target().to_string();
    let event_id = event.id.clone();
    let author_label = event.author_label();
    let inbound_text = recorded_watch_text(runner, agent_id, session_id, &event);
    let job: ResponseJob = Box::pin(async move {
        let inbound = opencrab_actions::NormalizedInbound {
            session_id: &sid,
            agent_id: &agent,
            sender_id: &event.pubkey,
            sender_name: &author_label,
            avatar_url: None,
            channel_id: None,
            pubkey: Some(&event.pubkey),
            text: &inbound_text,
            image_urls: &[],
            external_id: &event_id,
        };
        let effect = runtime
            .run_serialized(&sid, async {
                run_watch_turn(
                    &responder_runner,
                    &cli,
                    &admin,
                    &runtime,
                    &inbound,
                    caller,
                    &reply_target,
                    &prompt_suffix,
                    Some(&event_id),
                )
                .await
            })
            .await;
        apply_watch_effect(&responder_runner, &agent, &sid, &reply_target, &effect);
    });
    queues.enqueue(agent_id, session_id, permits, job);
}
