async fn send_frame(client: &InstanceClient, value: Value) -> bool {
    let inner = client.inner.lock().await;
    if inner.closed {
        return false;
    }
    client.write.lock().await.tx.send(value).is_ok()
}

async fn attach(
    client: &Arc<InstanceClient>,
    socket: &std::path::Path,
) -> Result<(), FrameError> {
    // A replacement cannot become visible until all old connection futures are dropped.
    if let Some(previous) = client.connection.lock().await.take() {
        let _ = previous.await;
    }
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|_| FrameError::Io)?;
    let (reader, writer) = stream.into_split();
    let (write_tx, write_rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(());
    let generation = {
        let mut inner = client.inner.lock().await;
        inner.generation = inner.generation.saturating_add(1);
        inner.closed = false;
        inner.stop = Some(stop_tx);
        inner.pending_said.clear();
        inner.pending_commands.clear();
        inner.expired_commands.clear();
        inner.pending_turn.clear();
        inner.live.clear();
        inner.generation
    };
    *client.write.lock().await = WriteOut { tx: write_tx };
    let hello_id = format!("hello:{}", client.instance_id);
    if !send_frame(
        client,
        hello_frame_with_capabilities_and_env(
            &hello_id,
            &client.instance_id,
            &client.agent_id,
            client.operations.as_ref(),
            client.runtime_capabilities,
            &client.run_env,
        ),
    )
    .await
    {
        return Err(FrameError::Io);
    }
    let (hello_tx, hello_rx) = oneshot::channel();
    {
        let mut inner = client.inner.lock().await;
        inner.pending_said.insert(
            hello_id,
            PendingSaid {
                kind: PendingKind::Hello,
                reply: hello_tx,
            },
        );
    }
    *client.connection.lock().await = Some(tokio::spawn(connection_loop(
        reader, writer, write_rx, client.clone(), generation, stop_rx,
    )));
    tracing::info!(instance_id = %client.instance_id, "hello");
    match hello_rx.await {
        Ok(SaidOutcome::Accepted { .. }) | Ok(SaidOutcome::NotAdmitted) => {
            tracing::info!(instance_id = %client.instance_id, "hello ok");
            Ok(())
        }
        // fail-loud（#894）: サーバの err_frame コードをそのまま報告する。従来は WireErr の
        // code を捨て、後続 EOF を read_loop が `close_all("disconnect")` に潰していたため、
        // 真因（config_digest_mismatch 等）が両側で不可視だった。
        Ok(SaidOutcome::WireErr { code, detail }) => {
            tracing::warn!(
                instance_id = %client.instance_id,
                reason = %code,
                detail = ?detail,
                "hello failed"
            );
            Err(FrameError::Io)
        }
        Ok(SaidOutcome::Disconnected | SaidOutcome::Opened { .. }) => {
            tracing::warn!(
                instance_id = %client.instance_id,
                reason = "disconnect",
                "hello failed"
            );
            Err(FrameError::Io)
        }
        Err(_) => {
            tracing::warn!(
                instance_id = %client.instance_id,
                reason = "hello_reply_dropped",
                "hello failed"
            );
            Err(FrameError::Io)
        }
    }
}

async fn reconnect_loop(client: Arc<InstanceClient>, socket: PathBuf) {
    let mut backoff = RECONNECT_MIN;
    loop {
        match attach(&client, &socket).await {
            Ok(()) => {
                tracing::info!(instance_id = %client.instance_id, "uds connected");
                backoff = RECONNECT_MIN;
                let notified = client.closed_notify.notified();
                if client.inner.lock().await.closed {
                    tracing::info!(instance_id = %client.instance_id, "uds closed during hello");
                } else {
                    notified.await;
                    tracing::info!(instance_id = %client.instance_id, "uds closed; reconnecting");
                }
            }
            Err(_) => {
                tracing::warn!(
                    instance_id = %client.instance_id,
                    "uds connect/hello failed"
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = backoff.saturating_mul(2).min(RECONNECT_MAX);
    }
}

async fn write_loop(mut writer: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<Value>) {
    while let Some(value) = rx.recv().await {
        if write_json(&mut writer, &value).await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

// Ordinary frames retain wire order; only responses may bypass a held handler.
const PENDING_FRAME_CAP: usize = 32;

async fn connection_loop(
    reader: tokio::net::unix::OwnedReadHalf,
    writer: OwnedWriteHalf,
    write_rx: mpsc::UnboundedReceiver<Value>,
    client: Arc<InstanceClient>,
    generation: u64,
    mut stop: tokio::sync::watch::Receiver<()>,
) {
    let (pending_tx, mut pending_rx) = mpsc::channel(PENDING_FRAME_CAP);
    let reason = {
        let dispatch = async {
            while let Some(msg) = pending_rx.recv().await {
                if handle_msg(&client, msg, generation).await {
                    break;
                }
            }
            "disconnect"
        };
        // These futures are owned, not detached tasks. Leaving this select drops the handler,
        // queued frames, reader and writer together. Effects already in flight stay uncertain.
        tokio::select! {
            biased;
            _ = stop.changed() => "disconnect",
            reason = read_loop(reader, &client, generation, pending_tx) => reason,
            reason = dispatch => reason,
            _ = write_loop(writer, write_rx) => "disconnect",
        }
    };
    close_all(&client, reason, generation).await;
}

async fn read_loop(
    mut reader: tokio::net::unix::OwnedReadHalf,
    client: &InstanceClient,
    generation: u64,
    pending: mpsc::Sender<CoreMsg>,
) -> &'static str {
    loop {
        // Kept alive across dispatcher polls: read_frame uses non-cancellation-safe read_exact.
        match read_frame(&mut reader).await {
            Ok(bytes) => match parse_frame_bytes(&bytes) {
                Ok(CoreMsg::Response(resp)) => {
                    handle_response(client, resp, generation).await;
                    if client.inner.lock().await.closed {
                        return "disconnect";
                    }
                }
                Ok(CoreMsg::Invalid { code: "response_invalid", .. }) => {
                    return "response_invalid";
                }
                Ok(msg) => {
                    if pending.try_send(msg).is_err() {
                        return "pending_frame_queue_full";
                    }
                }
                Err(FrameError::BadRequest) | Err(FrameError::TooLarge) => return "bad_request",
                Err(_) => return "disconnect",
            },
            Err(FrameError::TooLarge) => return "too_large",
            Err(_) => return "disconnect",
        }
    }
}
