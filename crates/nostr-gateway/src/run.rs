//! 1 instance の UDS client と watch lane。
//!
//! watch は bind ack の後にだけ起動する（DESIGN-NOSTRGATE §6 #16）。
//! 切断で child を止め、読取済み未送信は破棄する。再送はしない。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::client::{InstanceClient, PostRefuse, SaidOutcome};
use opencrab_gate_client::SayPolicy;
use tokio::sync::Notify;

use crate::config::{
    config_digest, parse_instance_config, watches_beyond_self, InstanceConfig, InstancePlacement,
    WatchFilter, WatchPlacement,
};
use crate::map::{
    bundle_id, classify_route, map_event, parse_watch_line, BundlePlace, Lane, Route, WatchEvent,
};
use crate::watch::{run_watch_loop, RESUBSCRIBE};

const BIND_POLL: Duration = Duration::from_millis(50);

#[derive(Default)]
struct SaidMetrics {
    store_error: AtomicU64,
    bad_request: AtomicU64,
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
    let client = InstanceClient::spawn_with_say_policy(
        socket,
        place.instance_id.clone(),
        place.revision,
        cfg.self_pubkey.clone(),
        digest,
        SayPolicy::RejectExternal,
    );
    let metrics = Arc::new(SaidMetrics::default());
    supervise_lanes(
        client.clone(),
        place.address.clone(),
        cfg,
        secret,
        nostaro_bin,
        metrics,
    );
    Ok(client)
}

fn supervise_lanes(
    client: Arc<InstanceClient>,
    address: String,
    cfg: InstanceConfig,
    secret: Option<Arc<String>>,
    nostaro_bin: PathBuf,
    metrics: Arc<SaidMetrics>,
) {
    tokio::spawn(async move {
        loop {
            wait_until_bound(&client, &address).await;
            tracing::info!(address = %address, "bind ack; starting watch");
            let cancel = Arc::new(Notify::new());
            let handles = start_lanes(
                client.clone(),
                address.clone(),
                cfg.clone(),
                secret.clone(),
                nostaro_bin.clone(),
                metrics.clone(),
                cancel.clone(),
            );
            wait_until_unbound(&client, &address).await;
            tracing::info!(address = %address, "binding lost; stopping watch");
            cancel.notify_waiters();
            for handle in handles {
                handle.abort();
            }
        }
    });
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

fn start_lanes(
    client: Arc<InstanceClient>,
    address: String,
    cfg: InstanceConfig,
    secret: Option<Arc<String>>,
    nostaro_bin: PathBuf,
    metrics: Arc<SaidMetrics>,
    cancel: Arc<Notify>,
) -> Vec<tokio::task::JoinHandle<()>> {
    if cfg.watches.is_empty() {
        return vec![spawn_lane(
            client,
            address,
            Lane::default_lane(),
            cfg.relays,
            cfg.filter,
            cfg.self_pubkey,
            None,
            secret,
            nostaro_bin,
            metrics,
            cancel,
        )];
    }
    cfg.watches
        .into_iter()
        .map(|watch| {
            let filter = watch.effective_filter().clone();
            spawn_lane(
                client.clone(),
                address.clone(),
                Lane::watch(watch.id),
                cfg.relays.clone(),
                filter,
                cfg.self_pubkey.clone(),
                Some(watch),
                secret.clone(),
                nostaro_bin.clone(),
                metrics.clone(),
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
    cancel: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    let beyond = watches_beyond_self(&filter);
    let interval = watch
        .as_ref()
        .map(|w| Duration::from_secs(w.interval_secs as u64));
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
                move |line| {
                    let pending = pending.clone();
                    let client = client.clone();
                    let address = address.clone();
                    let lane = lane.clone();
                    let self_pubkey = self_pubkey.clone();
                    let metrics = metrics.clone();
                    tokio::spawn(async move {
                        handle_line(
                            &client,
                            &address,
                            &lane,
                            &self_pubkey,
                            beyond,
                            &pending,
                            &metrics,
                            line,
                        )
                        .await;
                    });
                }
            })
            .await;
        };
        let flush_fut = async {
            let Some(interval) = interval else {
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
    line: String,
) {
    let Some(event) = parse_watch_line(&line) else {
        return;
    };
    let route = classify_route(&event, self_pubkey, beyond, lane);
    if route == Route::Bundle {
        pending.lock().await.push(event);
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

async fn flush_bundle(
    client: &InstanceClient,
    address: &str,
    lane: &Lane,
    self_pubkey: &str,
    beyond: bool,
    pending: &tokio::sync::Mutex<Vec<WatchEvent>>,
    metrics: &SaidMetrics,
) {
    let mut events = {
        let mut g = pending.lock().await;
        std::mem::take(&mut *g)
    };
    if events.is_empty() {
        return;
    }
    events.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
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
    for (i, event) in events.iter().enumerate() {
        let place = BundlePlace {
            bundle_id: bundle.clone(),
            index: (i as u32) + 1,
            count,
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
    match client
        .post_said_with_author(
            address,
            &mapped.origin,
            &mapped.author_id,
            &mapped.text,
            &[],
        )
        .await
    {
        Ok(outcome) => record_said_outcome(metrics, &mapped.origin, &outcome),
        Err(PostRefuse::NotReady) => {
            tracing::info!(origin = %mapped.origin, "said dropped; binding not ready");
        }
        Err(PostRefuse::Busy) => {
            tracing::info!(origin = %mapped.origin, "said refused; binding busy");
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
}
