//! 1 instance の UDS client と watch lane。
//!
//! watch は bind ack の後にだけ起動する（DESIGN-NOSTRGATE §6 #16）。
//! 切断で child を止め、読取済み未送信は破棄する。再送はしない。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::client::{InstanceClient, LiveEvent, PostRefuse, SaidOutcome};
use opencrab_gate_client::SayPolicy;
use tokio::sync::Notify;

use crate::config::{
    config_digest, mention_lane_filter, parse_instance_config, watches_beyond_self, InstanceConfig,
    InstancePlacement, WatchFilter, WatchPlacement,
};
use crate::dedup::SeenEvents;
use crate::map::{
    bundle_id, classify_route, map_event, normalize_author_id, parse_watch_line, BundlePlace, Lane,
    LaneKind, Route, WatchEvent,
};
use crate::post::{self, SayDelivery};
use crate::watch::{run_watch_loop, RESUBSCRIBE};

const BIND_POLL: Duration = Duration::from_millis(50);

/// 車線をまたぐ dedup TTL の下限。最大 watch interval を上回れないと、即時送出した
/// メンションと interval 後に flush される bundle の間で取りこぼす。
const DEDUP_TTL_FLOOR: Duration = Duration::from_secs(600);

/// 最大 watch interval の 2 倍と下限の大きい方＋余裕。bundle flush 窓を確実に跨ぐ。
fn dedup_ttl(cfg: &InstanceConfig) -> Duration {
    let max_interval = cfg
        .watches
        .iter()
        .map(|w| w.interval_secs as u64)
        .max()
        .unwrap_or(0);
    let by_interval = Duration::from_secs(max_interval.saturating_mul(2).saturating_add(60));
    by_interval.max(DEDUP_TTL_FLOOR)
}

#[derive(Default)]
struct SaidMetrics {
    store_error: AtomicU64,
    bad_request: AtomicU64,
    queue_full: AtomicU64,
    bundle_discarded: AtomicU64,
    deduped: AtomicU64,
    /// watch 車線が自分宛て #p を default 車線へ譲って捨てた回数（QC #10 対策・Defect A）。
    lane_deferred: AtomicU64,
    /// say → nostaro reply を投稿できた回数。
    say_posted: AtomicU64,
    /// say 投稿が失敗した回数（nostaro 非ゼロ / spawn 失敗 / timeout）。
    say_post_failed: AtomicU64,
    /// 返信先が無い（bundle/曖昧）say を新規ノート（standalone）として publish した回数（row292）。
    say_posted_standalone: AtomicU64,
}

pub fn spawn_instance(
    socket: PathBuf,
    place: &InstancePlacement,
    config_bytes: &[u8],
    secret: Option<Arc<String>>,
    nostaro_bin: PathBuf,
) -> anyhow::Result<Arc<InstanceClient>> {
    let cfg = parse_instance_config(config_bytes)?;
    let digest = config_digest(config_bytes);
    // 返信本文を nostaro reply で投稿するための relays だけの config（鍵は env 注入・config に載せない）。
    let post_config = post::post_config_path(&socket, &place.instance_id);
    post::write_relays_config(&post_config, &cfg.relays).map_err(|e| {
        anyhow::anyhow!(
            "nostaro post config を書けない ({}): {e}",
            post_config.display()
        )
    })?;
    let client = InstanceClient::spawn_with_say_policy(
        socket,
        place.instance_id.clone(),
        place.revision,
        cfg.self_pubkey.clone(),
        digest,
        SayPolicy::AcceptToLiveQueue,
    );
    let metrics = Arc::new(SaidMetrics::default());
    // 車線をまたぐ said dedup は instance 単位で共有する（全 default/watch 車線が同じセット）。
    let seen = Arc::new(SeenEvents::new(dedup_ttl(&cfg)));
    supervise_lanes(
        client.clone(),
        place.address.clone(),
        cfg,
        secret,
        nostaro_bin,
        post_config,
        metrics,
        seen,
    );
    Ok(client)
}

#[allow(clippy::too_many_arguments)]
fn supervise_lanes(
    client: Arc<InstanceClient>,
    address: String,
    cfg: InstanceConfig,
    secret: Option<Arc<String>>,
    nostaro_bin: PathBuf,
    post_config: PathBuf,
    metrics: Arc<SaidMetrics>,
    seen: Arc<SeenEvents>,
) {
    tokio::spawn(async move {
        loop {
            wait_until_bound(&client, &address).await;
            tracing::info!(address = %address, "bind ack; starting watch");
            let cancel = Arc::new(Notify::new());
            let mut handles = start_lanes(
                client.clone(),
                address.clone(),
                cfg.clone(),
                secret.clone(),
                nostaro_bin.clone(),
                metrics.clone(),
                seen.clone(),
                cancel.clone(),
            );
            // core からの say（返信本文）を消費して nostaro reply で投稿する consumer。
            handles.push(spawn_say_consumer(
                client.clone(),
                address.clone(),
                nostaro_bin.clone(),
                post_config.clone(),
                secret.clone(),
                metrics.clone(),
            ));
            wait_until_unbound(&client, &address).await;
            tracing::info!(address = %address, "binding lost; stopping watch");
            cancel.notify_waiters();
            for handle in handles {
                handle.abort();
            }
        }
    });
}

/// core からの say を live queue から取り出し、発端イベントへの e-tag reply として nostaro で
/// 投稿する。返信先があれば e-tag reply、無ければ（bundle/曖昧）新規ノート（standalone post）で
/// publish する（row292/#843: drop しない）。
fn spawn_say_consumer(
    client: Arc<InstanceClient>,
    address: String,
    nostaro_bin: PathBuf,
    post_config: PathBuf,
    secret: Option<Arc<String>>,
    metrics: Arc<SaidMetrics>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match client.next_live(&address).await {
                Some(LiveEvent::Message { text, reply_origin }) => {
                    let secret_ref = secret.as_ref().map(|s| s.as_str());
                    match post::deliver_say(
                        &nostaro_bin,
                        &post_config,
                        secret_ref,
                        reply_origin,
                        &text,
                    )
                    .await
                    {
                        SayDelivery::Posted => {
                            let n = metrics.say_posted.fetch_add(1, Ordering::Relaxed) + 1;
                            tracing::info!(address = %address, say_posted = n, "say posted as reply");
                        }
                        SayDelivery::PostedStandalone => {
                            let n = metrics
                                .say_posted_standalone
                                .fetch_add(1, Ordering::Relaxed)
                                + 1;
                            tracing::info!(
                                address = %address,
                                say_posted_standalone = n,
                                "say posted as standalone (no single reply target)"
                            );
                        }
                        SayDelivery::Failed(e) => {
                            let n = metrics.say_post_failed.fetch_add(1, Ordering::Relaxed) + 1;
                            tracing::warn!(address = %address, say_post_failed = n, error = %e, "say post failed");
                        }
                    }
                }
                // 切断。少し待って再試行（再接続後 next_live が再びブロックする）。
                Some(LiveEvent::Error { .. }) | None => {
                    tokio::time::sleep(BIND_POLL).await;
                }
                // Activity / CompletedNoReply は投稿対象ではない。
                Some(_) => {}
            }
        }
    })
}

async fn wait_until_bound(client: &InstanceClient, address: &str) {
    loop {
        if client.binding_for_address(address).await.is_some() {
            return;
        }
        tokio::time::sleep(BIND_POLL).await;
    }
}

async fn wait_until_unbound(client: &InstanceClient, address: &str) {
    loop {
        if client.binding_for_address(address).await.is_none() {
            return;
        }
        tokio::time::sleep(BIND_POLL).await;
    }
}

/// default(メンション)車線は常設。watch は追加車線。
struct LaneSpawn {
    lane: Lane,
    filter: WatchFilter,
    watch: Option<WatchPlacement>,
}

fn plan_lane_spawns(cfg: &InstanceConfig) -> Vec<LaneSpawn> {
    let mut planned = Vec::with_capacity(1 + cfg.watches.len());
    planned.push(LaneSpawn {
        lane: Lane::default_lane(),
        filter: mention_lane_filter(cfg),
        watch: None,
    });
    for watch in &cfg.watches {
        planned.push(LaneSpawn {
            lane: Lane::watch(watch.id),
            filter: watch.effective_filter().clone(),
            watch: Some(watch.clone()),
        });
    }
    planned
}

#[allow(clippy::too_many_arguments)]
fn start_lanes(
    client: Arc<InstanceClient>,
    address: String,
    cfg: InstanceConfig,
    secret: Option<Arc<String>>,
    nostaro_bin: PathBuf,
    metrics: Arc<SaidMetrics>,
    seen: Arc<SeenEvents>,
    cancel: Arc<Notify>,
) -> Vec<tokio::task::JoinHandle<()>> {
    plan_lane_spawns(&cfg)
        .into_iter()
        .map(|planned| {
            spawn_lane(
                client.clone(),
                address.clone(),
                planned.lane,
                cfg.relays.clone(),
                planned.filter,
                cfg.self_pubkey.clone(),
                planned.watch,
                secret.clone(),
                nostaro_bin.clone(),
                metrics.clone(),
                seen.clone(),
                cancel.clone(),
            )
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn spawn_lane(
    client: Arc<InstanceClient>,
    address: String,
    lane: Lane,
    relays: Vec<String>,
    filter: WatchFilter,
    self_pubkey: String,
    watch: Option<WatchPlacement>,
    secret: Option<Arc<String>>,
    nostaro_bin: PathBuf,
    metrics: Arc<SaidMetrics>,
    seen: Arc<SeenEvents>,
    cancel: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    let beyond = match lane.kind {
        LaneKind::Default => false,
        LaneKind::Watch { .. } => watches_beyond_self(&filter),
    };
    let flush = watch.as_ref().map(|w| {
        (
            Duration::from_secs(w.interval_secs as u64),
            w.max_items as usize,
        )
    });
    tokio::spawn(async move {
        let pending = Arc::new(tokio::sync::Mutex::new(Vec::<WatchEvent>::new()));
        let watch_fut = async {
            run_watch_loop(nostaro_bin, relays, filter, secret, RESUBSCRIBE, {
                let pending = pending.clone();
                let client = client.clone();
                let address = address.clone();
                let lane = lane.clone();
                let self_pubkey = self_pubkey.clone();
                let metrics = metrics.clone();
                let seen = seen.clone();
                move |line| {
                    let pending = pending.clone();
                    let client = client.clone();
                    let address = address.clone();
                    let lane = lane.clone();
                    let self_pubkey = self_pubkey.clone();
                    let metrics = metrics.clone();
                    let seen = seen.clone();
                    tokio::spawn(async move {
                        handle_line(
                            &client,
                            &address,
                            &lane,
                            &self_pubkey,
                            beyond,
                            &pending,
                            &metrics,
                            &seen,
                            line,
                        )
                        .await;
                    });
                }
            })
            .await;
        };
        let flush_fut = async {
            let Some((interval, max_items)) = flush else {
                std::future::pending::<()>().await;
                return;
            };
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                flush_bundle(
                    &client,
                    &address,
                    &lane,
                    &self_pubkey,
                    beyond,
                    &pending,
                    &metrics,
                    &seen,
                    max_items,
                )
                .await;
            }
        };
        tokio::select! {
            _ = watch_fut => {}
            _ = flush_fut => {}
            _ = cancel.notified() => {
                pending.lock().await.clear();
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn handle_line(
    client: &InstanceClient,
    address: &str,
    lane: &Lane,
    self_pubkey: &str,
    beyond: bool,
    pending: &tokio::sync::Mutex<Vec<WatchEvent>>,
    metrics: &SaidMetrics,
    seen: &SeenEvents,
    line: String,
) {
    let Some(event) = parse_watch_line(&line) else {
        return;
    };
    // Defect A（QC #10）: 自分宛て #p メンション/リプライは default(mention) 車線が `--npub` で
    // 必ず即時に拾う。watch 車線が同じイベントを先に握っても、ここで default 車線へ譲って捨てる。
    // これで owner がフォロイーにも含まれるときのレースで watch origin に取り込まれ、権限デバウンスへ
    // 降格する取りこぼしを防ぐ（default 車線の即時処理へ一本化する）。
    if should_defer_to_default_lane(lane, &event, self_pubkey) {
        let n = metrics.lane_deferred.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::debug!(
            id = %event.id,
            lane_deferred = n,
            "watch lane defers self-mention to default lane"
        );
        return;
    }
    let route = classify_route(&event, self_pubkey, beyond, lane);
    if route == Route::Bundle {
        // bundle は flush 時に dedup する（manifest の count/origins を一貫させるため）。
        pending.lock().await.push(event);
        return;
    }
    // 即時送出はここで event_id を握る。別車線が先に握っていたら二重 said を避ける。
    if !claim_or_skip(seen, metrics, &event.id) {
        return;
    }
    send_mapped(
        client,
        address,
        lane,
        self_pubkey,
        beyond,
        &event,
        None,
        metrics,
    )
    .await;
}

/// Defect A（QC #10）: watch(timeline) 車線が受けたイベントを default(mention) 車線へ譲るか。
///
/// 自分宛て `#p` のメンション/リプライは、default 車線が `mention_lane_filter` の `--npub` で必ず即時に
/// 拾う。owner がフォロイーにも含まれると同じイベントが両車線へ届き、watch 車線が先に握ると watch origin
/// で取り込まれて権限デバウンスへ降格する（即応の取りこぼし）。watch 車線側をここで捨てて default 車線の
/// 即時処理へ一本化する。default 車線・自分宛てでないイベントは対象外。
fn should_defer_to_default_lane(lane: &Lane, event: &WatchEvent, self_pubkey: &str) -> bool {
    matches!(lane.kind, LaneKind::Watch { .. })
        && crate::map::is_self_p_tag_mention(event, self_pubkey)
}

/// event_id を握れたら `true`。hex 化できる id のみ dedup 対象（それ以外は send_mapped が落とす）。
/// 既に握られていたら dedup メトリクスを上げて `false` を返す。
fn claim_or_skip(seen: &SeenEvents, metrics: &SaidMetrics, raw_event_id: &str) -> bool {
    let Some(event_id) = normalize_author_id(raw_event_id) else {
        // 非 hex は dedup キーにできない。send_mapped 側の map_event が落とす。
        return true;
    };
    if seen.claim(&event_id) {
        return true;
    }
    let n = metrics.deduped.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::info!(event_id = %event_id, deduped = n, "said skipped; event already handled by another lane");
    false
}

struct BundleWindow {
    events: Vec<WatchEvent>,
    discarded: usize,
}

/// interval 内のイベントを created_at / id 順に並べ、新しい方から `max_items` 件残す。
fn take_bundle_window(mut events: Vec<WatchEvent>, max_items: usize) -> BundleWindow {
    events.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    let discarded = events.len().saturating_sub(max_items);
    if discarded > 0 {
        events.drain(..discarded);
    }
    BundleWindow { events, discarded }
}

fn record_bundle_discarded(metrics: &SaidMetrics, discarded: usize, kept: usize, max_items: usize) {
    if discarded == 0 {
        return;
    }
    let n = metrics
        .bundle_discarded
        .fetch_add(discarded as u64, Ordering::Relaxed)
        + discarded as u64;
    tracing::warn!(
        discarded,
        kept,
        max_items,
        bundle_discarded = n,
        "bundle trimmed; older items dropped"
    );
}

#[allow(clippy::too_many_arguments)]
async fn flush_bundle(
    client: &InstanceClient,
    address: &str,
    lane: &Lane,
    self_pubkey: &str,
    beyond: bool,
    pending: &tokio::sync::Mutex<Vec<WatchEvent>>,
    metrics: &SaidMetrics,
    seen: &SeenEvents,
    max_items: usize,
) {
    let events = {
        let mut g = pending.lock().await;
        std::mem::take(&mut *g)
    };
    if events.is_empty() {
        return;
    }
    let window = take_bundle_window(events, max_items);
    record_bundle_discarded(metrics, window.discarded, window.events.len(), max_items);
    // 別車線（default 即時など）が既に送ったイベントを manifest 構築の前に除く。
    // ここで削ると bundle_id / origins / count が実送信分と一致する。
    let events: Vec<WatchEvent> = window
        .events
        .into_iter()
        .filter(|e| claim_or_skip(seen, metrics, &e.id))
        .collect();
    if events.is_empty() {
        return;
    }
    let Some(watch_id) = lane.watch_id() else {
        tracing::warn!("bundle dropped; default lane has no watch_id");
        return;
    };
    let Some(binding_id) = client.binding_for_address(address).await else {
        tracing::info!(count = events.len(), "bundle dropped; binding not ready");
        return;
    };
    let ids: Vec<String> = events
        .iter()
        .filter_map(|e| crate::map::normalize_author_id(&e.id))
        .collect();
    if ids.len() != events.len() {
        tracing::warn!(
            kept = ids.len(),
            total = events.len(),
            "bundle dropped; event id not hex"
        );
        return;
    }
    let bundle = bundle_id(&binding_id, watch_id, &ids);
    let count = events.len() as u32;
    let lane_for_origin = Lane::watch(watch_id);
    let origins: Vec<String> = ids
        .iter()
        .map(|id| crate::map::decisive_origin(&lane_for_origin, id))
        .collect();
    for (i, event) in events.iter().enumerate() {
        let place = BundlePlace {
            bundle_id: bundle.clone(),
            index: (i as u32) + 1,
            count,
            origins: origins.clone(),
        };
        send_mapped(
            client,
            address,
            lane,
            self_pubkey,
            beyond,
            event,
            Some(&place),
            metrics,
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_mapped(
    client: &InstanceClient,
    address: &str,
    lane: &Lane,
    self_pubkey: &str,
    beyond: bool,
    event: &WatchEvent,
    bundle: Option<&BundlePlace>,
    metrics: &SaidMetrics,
) {
    let Some(mapped) = map_event(event, self_pubkey, beyond, lane, bundle) else {
        tracing::warn!(id = %event.id, "said dropped; author or event id is not hex");
        return;
    };
    let post = if bundle.is_some() {
        client
            .post_said_receipt(
                address,
                &mapped.origin,
                &mapped.author_id,
                &mapped.text,
                &[],
            )
            .await
    } else {
        client
            .post_said_with_author(
                address,
                &mapped.origin,
                &mapped.author_id,
                &mapped.text,
                &[],
            )
            .await
    };
    match post {
        Ok(outcome) => record_said_outcome(metrics, &mapped.origin, &outcome),
        Err(PostRefuse::NotReady) => {
            tracing::info!(origin = %mapped.origin, "said dropped; binding not ready");
        }
        Err(PostRefuse::Busy) => {
            let n = metrics.queue_full.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::info!(origin = %mapped.origin, queue_full = n, "said refused; binding busy");
        }
    }
}

fn record_said_outcome(metrics: &SaidMetrics, origin: &str, outcome: &SaidOutcome) {
    match outcome {
        SaidOutcome::Accepted { seq } => {
            tracing::info!(origin, seq, "said accepted");
        }
        SaidOutcome::NotAdmitted => {
            tracing::info!(origin, "said not admitted");
        }
        SaidOutcome::Disconnected => {
            tracing::info!(origin, "said disconnected");
        }
        SaidOutcome::WireErr { code, detail } => {
            if code == "store_error" {
                let n = metrics.store_error.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(origin, code, ?detail, store_error = n, "said wire err");
            } else if code == "bad_request" {
                let n = metrics.bad_request.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(origin, code, ?detail, bad_request = n, "said wire err");
            } else {
                tracing::warn!(origin, code, ?detail, "said wire err");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::plan_watch_args;

    #[test]
    fn watches_present_still_spawns_mention_keyword_lane() {
        let self_pk = "aa".repeat(32);
        let cfg = InstanceConfig {
            relays: vec!["wss://example.invalid".into()],
            filter: WatchFilter::default(),
            self_pubkey: self_pk.clone(),
            name: Some("crab".into()),
            watches: vec![WatchPlacement {
                id: 3,
                interval_secs: 120,
                max_items: crate::config::DEFAULT_BUNDLE_MAX_ITEMS,
                filter: WatchFilter {
                    authors: vec!["npub1watched".into()],
                    ..WatchFilter::default()
                },
                filter_json: None,
            }],
            delivery_mode: None,
        };
        let planned = plan_lane_spawns(&cfg);
        assert_eq!(planned.len(), 2, "mention + watch");
        assert_eq!(planned[0].lane, Lane::default_lane());
        assert!(planned[0].watch.is_none());
        let mention_args = plan_watch_args(&cfg.relays, &planned[0].filter);
        assert_eq!(mention_args[0], "watch");
        assert!(
            mention_args.contains(&"--keyword=crab".to_string()),
            "name keyword missing: {mention_args:?}"
        );
        assert!(
            mention_args.contains(&format!("--npub={self_pk}")),
            "p-tag target missing: {mention_args:?}"
        );
        assert!(
            !mention_args.contains(&format!("--keyword={self_pk}")),
            "hex pubkey must not be a keyword: {mention_args:?}"
        );
        assert!(
            mention_args.contains(&"--kind=1".to_string()),
            "{mention_args:?}"
        );
        assert!(
            mention_args.contains(&"--kind=7".to_string()),
            "{mention_args:?}"
        );
        assert!(
            !mention_args.iter().any(|a| a.starts_with("--author=")),
            "mention lane must not take watch authors: {mention_args:?}"
        );
        let keyword_count = mention_args
            .iter()
            .filter(|a| a.starts_with("--keyword="))
            .count();
        assert_eq!(keyword_count, 1, "{mention_args:?}");
        assert_eq!(planned[1].lane, Lane::watch(3));
        let watch_args = plan_watch_args(&cfg.relays, &planned[1].filter);
        assert!(
            watch_args.contains(&"--author=npub1watched".to_string()),
            "{watch_args:?}"
        );
        assert!(
            !watch_args.iter().any(|a| a.starts_with("--keyword=")),
            "timeline lane must not inherit mention keywords: {watch_args:?}"
        );
        assert!(
            !watch_args.iter().any(|a| a.starts_with("--npub=")),
            "timeline lane must not inherit mention npub: {watch_args:?}"
        );
    }

    #[test]
    fn claim_or_skip_dedups_hex_ids_and_counts() {
        let metrics = SaidMetrics::default();
        let seen = SeenEvents::new(Duration::from_secs(600));
        let id = "aa".repeat(32);
        assert!(claim_or_skip(&seen, &metrics, &id), "初回は送る");
        assert!(!claim_or_skip(&seen, &metrics, &id), "二度目は重複で落とす");
        assert_eq!(metrics.deduped.load(Ordering::Relaxed), 1);
        // 別 event_id は独立に送れる。
        assert!(claim_or_skip(&seen, &metrics, &"bb".repeat(32)));
        assert_eq!(metrics.deduped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn claim_or_skip_passes_non_hex_without_dedup() {
        let metrics = SaidMetrics::default();
        let seen = SeenEvents::new(Duration::from_secs(600));
        // 非 hex は dedup キーにできないので常に通す（下流 map_event が落とす）。
        assert!(claim_or_skip(&seen, &metrics, "not-hex"));
        assert!(claim_or_skip(&seen, &metrics, "not-hex"));
        assert_eq!(metrics.deduped.load(Ordering::Relaxed), 0);
    }

    fn watch_event(kind: u32, tags: Vec<Vec<String>>) -> WatchEvent {
        WatchEvent {
            id: "aa".repeat(32),
            pubkey: "22".repeat(32),
            npub: None,
            note_id: Some("note1abc".into()),
            created_at: 1,
            kind,
            content: "hi".into(),
            tags,
        }
    }

    // Defect A（QC #10）: watch 車線は自分宛て #p を default 車線へ譲る。default 車線と
    // 他人宛て/#p 無しは譲らない。
    #[test]
    fn watch_lane_defers_self_p_tag_only() {
        let self_pk = "11".repeat(32);
        let self_mention = watch_event(1, vec![vec!["p".into(), self_pk.clone()]]);
        // watch 車線 × 自分宛て → 譲る（default 車線の即時処理へ一本化）。
        assert!(should_defer_to_default_lane(
            &Lane::watch(4),
            &self_mention,
            &self_pk
        ));
        // default 車線は決して譲らない（自分がここで即時処理する）。
        assert!(!should_defer_to_default_lane(
            &Lane::default_lane(),
            &self_mention,
            &self_pk
        ));
        // watch 車線 × 他人宛て → 譲らない（timeline の担当）。
        let other = watch_event(1, vec![vec!["p".into(), "99".repeat(32)]]);
        assert!(!should_defer_to_default_lane(
            &Lane::watch(4),
            &other,
            &self_pk
        ));
        // watch 車線 × #p 無しリプライ → 譲らない（default 車線の --npub では拾えない）。
        let e_only = watch_event(1, vec![vec!["e".into(), "bb".repeat(32)]]);
        assert!(!should_defer_to_default_lane(
            &Lane::watch(4),
            &e_only,
            &self_pk
        ));
    }

    #[test]
    fn dedup_ttl_covers_max_watch_interval() {
        let mut cfg = InstanceConfig {
            relays: vec!["wss://example.invalid".into()],
            filter: WatchFilter::default(),
            self_pubkey: "aa".repeat(32),
            name: None,
            watches: vec![],
            delivery_mode: None,
        };
        // watch 無しは下限。
        assert_eq!(dedup_ttl(&cfg), DEDUP_TTL_FLOOR);
        // 大きい interval は 2 倍＋余裕で下限を超える。
        cfg.watches = vec![WatchPlacement {
            id: 1,
            interval_secs: 3600,
            max_items: crate::config::DEFAULT_BUNDLE_MAX_ITEMS,
            filter: WatchFilter::default(),
            filter_json: None,
        }];
        let ttl = dedup_ttl(&cfg);
        assert!(
            ttl >= Duration::from_secs(3600 * 2),
            "TTL は最大 interval の 2 倍以上: {ttl:?}"
        );
    }

    #[test]
    fn store_error_and_bad_request_are_counted() {
        let metrics = SaidMetrics::default();
        record_said_outcome(
            &metrics,
            "o1",
            &SaidOutcome::WireErr {
                code: "store_error".into(),
                detail: None,
            },
        );
        record_said_outcome(
            &metrics,
            "o2",
            &SaidOutcome::WireErr {
                code: "bad_request".into(),
                detail: Some("anchor".into()),
            },
        );
        record_said_outcome(
            &metrics,
            "o3",
            &SaidOutcome::WireErr {
                code: "store_error".into(),
                detail: None,
            },
        );
        record_said_outcome(&metrics, "o4", &SaidOutcome::NotAdmitted);
        assert_eq!(metrics.store_error.load(Ordering::Relaxed), 2);
        assert_eq!(metrics.bad_request.load(Ordering::Relaxed), 1);
    }

    fn hex_id(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    fn timeline_event(n: u8, created_at: i64) -> WatchEvent {
        WatchEvent {
            id: hex_id(n),
            pubkey: hex_id(0xaa),
            npub: None,
            note_id: None,
            created_at,
            kind: 1,
            content: format!("e{n}"),
            tags: vec![],
        }
    }

    fn prepared_manifest(watch_id: i64, events: &[WatchEvent]) -> (String, Vec<String>, u32) {
        let ids: Vec<String> = events
            .iter()
            .map(|e| crate::map::normalize_author_id(&e.id).expect("hex id"))
            .collect();
        let lane = Lane::watch(watch_id);
        let origins: Vec<String> = ids
            .iter()
            .map(|id| crate::map::decisive_origin(&lane, id))
            .collect();
        (
            bundle_id("bind-1", watch_id, &ids),
            origins,
            events.len() as u32,
        )
    }

    #[test]
    fn bundle_window_keeps_all_when_at_or_under_max() {
        let events = vec![
            timeline_event(2, 20),
            timeline_event(1, 10),
            timeline_event(3, 30),
        ];
        let window = take_bundle_window(events, 50);
        assert_eq!(window.discarded, 0);
        assert_eq!(
            window
                .events
                .iter()
                .map(|e| e.created_at)
                .collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
    }

    #[test]
    fn bundle_window_keeps_newest_50_when_over_max() {
        let mut events: Vec<WatchEvent> = (1u8..=60)
            .map(|n| timeline_event(n, i64::from(n)))
            .collect();
        events.reverse();
        let window = take_bundle_window(events, 50);
        assert_eq!(window.discarded, 10);
        assert_eq!(window.events.len(), 50);
        let kept: Vec<i64> = window.events.iter().map(|e| e.created_at).collect();
        assert_eq!(kept.first().copied(), Some(11));
        assert_eq!(kept.last().copied(), Some(60));
        assert_eq!(kept, (11..=60).collect::<Vec<i64>>());
    }

    #[test]
    fn bundle_discarded_count_is_recorded() {
        let metrics = SaidMetrics::default();
        let events: Vec<WatchEvent> = (1u8..=60)
            .map(|n| timeline_event(n, i64::from(n)))
            .collect();
        let window = take_bundle_window(events, 50);
        record_bundle_discarded(&metrics, window.discarded, window.events.len(), 50);
        assert_eq!(metrics.bundle_discarded.load(Ordering::Relaxed), 10);
        record_bundle_discarded(&metrics, 3, 50, 50);
        assert_eq!(metrics.bundle_discarded.load(Ordering::Relaxed), 13);
        record_bundle_discarded(&metrics, 0, 2, 50);
        assert_eq!(metrics.bundle_discarded.load(Ordering::Relaxed), 13);
    }

    #[test]
    fn capped_bundle_manifest_matches_coordinator_contract() {
        let events: Vec<WatchEvent> = (1u8..=60)
            .map(|n| timeline_event(n, i64::from(n)))
            .collect();
        let window = take_bundle_window(events, 50);
        let (bundle, origins, count) = prepared_manifest(17, &window.events);
        assert_eq!(count, 50);
        assert_eq!(origins.len(), count as usize);
        let dropped = timeline_event(1, 1);
        let dropped_origin = crate::map::decisive_origin(&Lane::watch(17), &dropped.id);
        assert!(
            !origins.contains(&dropped_origin),
            "discarded origin must not enter manifest"
        );
        let kept_ids: Vec<String> = window
            .events
            .iter()
            .map(|e| crate::map::normalize_author_id(&e.id).unwrap())
            .collect();
        assert_eq!(bundle, bundle_id("bind-1", 17, &kept_ids));
        let all_ids: Vec<String> = (1u8..=60)
            .map(|n| crate::map::normalize_author_id(&hex_id(n)).unwrap())
            .collect();
        assert_ne!(
            bundle,
            bundle_id("bind-1", 17, &all_ids),
            "bundle_id must not include discarded ids"
        );
        for (i, event) in window.events.iter().enumerate() {
            let place = BundlePlace {
                bundle_id: bundle.clone(),
                index: (i as u32) + 1,
                count,
                origins: origins.clone(),
            };
            assert!(place.index >= 1 && (place.index as usize) <= origins.len());
            assert_eq!(place.origins.len(), place.count as usize);
            let mapped = map_event(
                event,
                &"11".repeat(32),
                true,
                &Lane::watch(17),
                Some(&place),
            )
            .expect("map");
            assert!(mapped
                .text
                .contains(&crate::map::bundle_members_line(&origins)));
            assert!(mapped.text.contains(&format!("\"count\":{count}")));
            assert!(!mapped.text.contains(&dropped_origin));
        }
    }
}
