fn spawn_say_consumer(
    client: Arc<InstanceClient>,
    address: String,
    _nostaro_bin: PathBuf,
    _post_config: PathBuf,
    secret: Option<Arc<String>>,
    metrics: Arc<SaidMetrics>,
    dry_run: bool,
    relays: Vec<String>,
    emission_ledger: Arc<opencrab_gate_client::emission::EmissionLedger>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut pending_delivery_bindings = std::collections::HashMap::<String, String>::new();
        loop {
            match client.next_live(&address).await {
                Some(LiveEvent::Message {
                    delivery_id,
                    binding_id,
                    payload_digest,
                    delivery_guarantee,
                    adapter_protocol_digest,
                    current_adapter_protocol_digest,
                    text,
                    reply_origin,
                }) => {
                    let (event_id, prepared) = if dry_run {
                        let bytes = serde_json::json!({"content":text,"reply":reply_origin})
                            .to_string()
                            .into_bytes();
                        let id = Sha256::digest(&bytes)
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>();
                        (id, bytes)
                    } else {
                        let Some(secret) = secret.as_deref() else {
                            tracing::error!("nostr delivery secret missing");
                            continue;
                        };
                        match post::prepare_signed_event(secret, reply_origin.as_deref(), &text) {
                            Ok(prepared) => prepared,
                            Err(error) => {
                                tracing::error!(%error, "nostr event prepare failed");
                                continue;
                            }
                        }
                    };
                    let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
                    let row = match emission_ledger.prepare(
                        &binding_id,
                        &delivery_id,
                        &payload_digest,
                        delivery_guarantee.as_str(),
                        &event_id,
                        &prepared,
                        &adapter_protocol_digest,
                        now,
                        None,
                    ) {
                        Ok(row) => row,
                        Err(error) => {
                            tracing::error!(%error, "delivery ledger conflict");
                            continue;
                        }
                    };
                    pending_delivery_bindings.insert(delivery_id.clone(), binding_id.clone());
                    let outcome = match row.state {
                        opencrab_gate_client::emission::EmissionState::Receipted => {
                            opencrab_gate_client::DeliveryOutcome::Receipted
                        }
                        opencrab_gate_client::emission::EmissionState::Failed => {
                            opencrab_gate_client::DeliveryOutcome::Failed
                        }
                        opencrab_gate_client::emission::EmissionState::Indeterminate
                        | opencrab_gate_client::emission::EmissionState::OperatorBlocked => {
                            opencrab_gate_client::DeliveryOutcome::Indeterminate
                        }
                        opencrab_gate_client::emission::EmissionState::Prepared
                            if adapter_protocol_digest != current_adapter_protocol_digest =>
                        {
                            let _ = emission_ledger.terminal(
                                &binding_id,
                                &delivery_id,
                                opencrab_gate_client::emission::EmissionState::OperatorBlocked,
                                None,
                                now,
                            );
                            opencrab_gate_client::DeliveryOutcome::Indeterminate
                        }
                        opencrab_gate_client::emission::EmissionState::Prepared => {
                            let _ = emission_ledger.mark_external_attempted(
                                &binding_id,
                                &delivery_id,
                                now,
                            );
                            let sent = if dry_run {
                                SayDelivery::Posted
                            } else {
                                post::publish_signed_event(&relays, &row.prepared_request).await
                            };
                            match sent {
                                SayDelivery::Posted | SayDelivery::PostedStandalone => {
                                    let _ = emission_ledger.terminal(
                                        &binding_id,
                                        &delivery_id,
                                        opencrab_gate_client::emission::EmissionState::Receipted,
                                        Some(&row.request_identity),
                                        now,
                                    );
                                    metrics.say_posted.fetch_add(1, Ordering::Relaxed);
                                    opencrab_gate_client::DeliveryOutcome::Receipted
                                }
                                SayDelivery::Failed(error) if error == "indeterminate" => {
                                    // Nostr retries the exact persisted signed event; logical identity remains one.
                                    tracing::warn!(%error, "nostr publish ambiguous; pending exact-event retry");
                                    continue;
                                }
                                SayDelivery::Failed(error) => {
                                    tracing::warn!(%error, "nostr publish rejected");
                                    let _ = emission_ledger.terminal(
                                        &binding_id,
                                        &delivery_id,
                                        opencrab_gate_client::emission::EmissionState::Failed,
                                        None,
                                        now,
                                    );
                                    opencrab_gate_client::DeliveryOutcome::Failed
                                }
                            }
                        }
                    };
                    let _ = client.complete_delivery(&delivery_id, outcome).await;
                }
                Some(LiveEvent::DeliveryAcknowledged { delivery_id, .. }) => {
                    if let Some(binding_id) = pending_delivery_bindings.remove(&delivery_id) {
                        let _ = emission_ledger.acknowledge_core(
                            &binding_id,
                            &delivery_id,
                            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
                        );
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

