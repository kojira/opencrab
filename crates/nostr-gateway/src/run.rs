//! 1 instance の UDS client と watch lane。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::client::InstanceClient;
use opencrab_gate_client::SayPolicy;

use crate::config::{
    config_digest, parse_instance_config, watches_beyond_self, InstanceConfig, InstancePlacement,
    WatchFilter, WatchPlacement,
};
use crate::map::{
    bundle_id, classify_route, map_event, parse_watch_line, BundlePlace, Lane, Route, WatchEvent,
};
use crate::watch::{run_watch_loop, RESUBSCRIBE};

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
    start_lanes(
        client.clone(),
        place.address.clone(),
        cfg,
        secret,
        nostaro_bin,
    );
    Ok(client)
}

fn start_lanes(
    client: Arc<InstanceClient>,
    address: String,
    cfg: InstanceConfig,
    secret: Option<Arc<String>>,
    nostaro_bin: PathBuf,
) {
    if cfg.watches.is_empty() {
        spawn_lane(
            client,
            address,
            Lane::default_lane(),
            cfg.relays,
            cfg.filter,
            cfg.self_pubkey,
            None,
            secret,
            nostaro_bin,
        );
        return;
    }
    for watch in cfg.watches {
        spawn_lane(
            client.clone(),
            address.clone(),
            Lane::watch(watch.id),
            cfg.relays.clone(),
            watch.filter.clone(),
            cfg.self_pubkey.clone(),
            Some(watch),
            secret.clone(),
            nostaro_bin.clone(),
        );
    }
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
) {
    let beyond = watches_beyond_self(&filter);
    let interval = watch
        .as_ref()
        .map(|w| Duration::from_secs(w.interval_secs as u64));
    tokio::spawn(async move {
        let pending = Arc::new(tokio::sync::Mutex::new(Vec::<WatchEvent>::new()));
        if let Some(interval) = interval {
            let flush_client = client.clone();
            let flush_address = address.clone();
            let flush_lane = lane.clone();
            let flush_self = self_pubkey.clone();
            let flush_pending = pending.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(interval);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    flush_bundle(
                        &flush_client,
                        &flush_address,
                        &flush_lane,
                        &flush_self,
                        beyond,
                        &flush_pending,
                    )
                    .await;
                }
            });
        }
        run_watch_loop(
            nostaro_bin,
            relays,
            filter,
            secret,
            RESUBSCRIBE,
            move |line| {
                let pending = pending.clone();
                let client = client.clone();
                let address = address.clone();
                let lane = lane.clone();
                let self_pubkey = self_pubkey.clone();
                tokio::spawn(async move {
                    handle_line(&client, &address, &lane, &self_pubkey, beyond, &pending, line)
                        .await;
                });
            },
        )
        .await;
    });
}

async fn handle_line(
    client: &InstanceClient,
    address: &str,
    lane: &Lane,
    self_pubkey: &str,
    beyond: bool,
    pending: &tokio::sync::Mutex<Vec<WatchEvent>>,
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
    send_mapped(client, address, lane, self_pubkey, beyond, &event, None).await;
}

async fn flush_bundle(
    client: &InstanceClient,
    address: &str,
    lane: &Lane,
    self_pubkey: &str,
    beyond: bool,
    pending: &tokio::sync::Mutex<Vec<WatchEvent>>,
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
        return;
    };
    let Some(binding_id) = client.binding_for_address(address).await else {
        return;
    };
    let ids: Vec<String> = events.iter().filter_map(|e| crate::map::normalize_author_id(&e.id)).collect();
    if ids.len() != events.len() {
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
        )
        .await;
    }
}

async fn send_mapped(
    client: &InstanceClient,
    address: &str,
    lane: &Lane,
    self_pubkey: &str,
    beyond: bool,
    event: &WatchEvent,
    bundle: Option<&BundlePlace>,
) {
    let Some(mapped) = map_event(event, self_pubkey, beyond, lane, bundle) else {
        return;
    };
    match client
        .post_said_with_author(address, &mapped.origin, &mapped.author_id, &mapped.text, &[])
        .await
    {
        Ok(_) => {}
        Err(_) => {
            tracing::info!(origin = %mapped.origin, "said dropped; binding not ready");
        }
    }
}
