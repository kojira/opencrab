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
use crate::harness::HarnessOverrides;
use crate::map::{
    bundle_id, classify_route, map_event, normalize_author_id, parse_watch_line, BundlePlace, Lane,
    LaneKind, Route, WatchEvent,
};
use crate::post::{self, SayDelivery};
use crate::watch::{run_fake_watch_once, run_watch_loop, RESUBSCRIBE};

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
    overrides: HarnessOverrides,
) -> anyhow::Result<Arc<InstanceClient>> {
    if overrides.is_active() {
        tracing::warn!(
            fake_watch = ?overrides.fake_watch,
            dry_run = overrides.dry_run,
            instance = %place.instance_id,
            "QC harness overrides ACTIVE — this is NOT a production path"
        );
    }
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
    // DI 能力宣言（§9.2）と invoke handler を hello に載せて接続する。invoke は nostaro CLI 実行へ
    // 写す（reply/reaction/repost/follow/unfollow/kind0/upload/resolve）。秘密鍵は env 注入のみ。
    let invoke_handler: Arc<dyn opencrab_gate_client::InvokeHandler> =
        Arc::new(crate::ops::NostrInvokeHandler::new(
            nostaro_bin.clone(),
            post_config.clone(),
            secret.as_ref().map(|s| s.as_str().to_string()),
            overrides.dry_run,
        ));
    let client = InstanceClient::spawn_with_operations(
        socket,
        place.instance_id.clone(),
        place.revision,
        cfg.self_pubkey.clone(),
        digest,
        SayPolicy::AcceptToLiveQueue,
        Some(crate::ops::operation_declarations()),
        invoke_handler,
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
        overrides,
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
    overrides: HarnessOverrides,
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
                overrides.fake_watch.clone(),
            );
            // core からの say（返信本文）を消費して nostaro post で publish する consumer。
            handles.push(spawn_say_consumer(
                client.clone(),
                address.clone(),
                nostaro_bin.clone(),
                post_config.clone(),
                secret.clone(),
                metrics.clone(),
                overrides.dry_run,
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
    dry_run: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match client.next_live(&address).await {
                Some(LiveEvent::Message {
                    text, reply_origin, ..
                }) => {
                    let secret_ref = secret.as_ref().map(|s| s.as_str());
                    match post::deliver_say(
                        &nostaro_bin,
                        &post_config,
                        secret_ref,
                        reply_origin,
                        &text,
                        dry_run,
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
    fake_watch: Option<PathBuf>,
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
                fake_watch.clone(),
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
    fake_watch: Option<PathBuf>,
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
        // 1 行受信ごとに handle_line を回すコールバック（実 watch・偽 watch 共通）。
        let on_line = {
            let pending = pending.clone();
            let client = client.clone();
            let address = address.clone();
            let lane = lane.clone();
            let self_pubkey = self_pubkey.clone();
            let metrics = metrics.clone();
            let seen = seen.clone();
            move |line: String| {
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
        };
        let watch_fut = async {
            // QC ハーネス: fake_watch が指定されていれば nostaro を spawn せず fixture を流す。
            // 指定が無ければ（production）従来どおり実 nostaro watch を回す。
            if let Some(fixture) = fake_watch {
                if let Err(e) = run_fake_watch_once(&fixture, on_line).await {
                    tracing::error!(error = %e, "fake watch failed");
                }
            } else {
                run_watch_loop(nostaro_bin, relays, filter, secret, RESUBSCRIBE, on_line).await;
            }
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
    // 即時送出はここで event_id を握る（二重 said 回避）。ただし core が Accepted しなければ
    // claim を解除し、別車線 copy が後追いで届くようにする（claim-after-accept・#839 NIT の実害化対策）。
    if !claim_or_skip(seen, metrics, &event.id) {
        return;
    }
    let accepted = send_mapped(
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
    if !accepted {
        if let Some(id) = normalize_author_id(&event.id) {
            seen.release(&id);
        }
    }
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
        let accepted = send_mapped(
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
        if !accepted {
            if let Some(id) = crate::map::normalize_author_id(&event.id) {
                seen.release(&id);
            }
        }
    }
}

/// said を送る。`true` は core が **Accepted** した場合のみ（claim を保持してよい）。
/// それ以外（WireErr=bad_request 等 / NotAdmitted / NotReady / Busy / map 失敗）は `false` を返し、
/// 呼び出し側が claim を解除して別車線 copy の取りこぼしを防ぐ（claim-after-accept・#839 NIT）。
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
) -> bool {
    let Some(mapped) = map_event(event, self_pubkey, beyond, lane, bundle) else {
        tracing::warn!(id = %event.id, "said dropped; author or event id is not hex");
        return false;
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
        Ok(outcome) => {
            record_said_outcome(metrics, &mapped.origin, &outcome);
            matches!(outcome, SaidOutcome::Accepted { .. })
        }
        Err(PostRefuse::NotReady) => {
            tracing::info!(origin = %mapped.origin, "said dropped; binding not ready");
            false
        }
        Err(PostRefuse::Busy) => {
            let n = metrics.queue_full.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::info!(origin = %mapped.origin, queue_full = n, "said refused; binding busy");
            false
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
#[path = "run/tests.rs"]
mod tests;
