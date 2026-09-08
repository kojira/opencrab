async fn send_frame(client: &InstanceClient, value: Value) -> bool {
    client.write.lock().await.tx.send(value).is_ok()
}

async fn attach(
    client: &Arc<InstanceClient>,
    socket: &std::path::Path,
    revision: u64,
    config_digest: &str,
) -> Result<(), FrameError> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|_| FrameError::Io)?;
    let (reader, writer) = stream.into_split();
    let (write_tx, write_rx) = mpsc::unbounded_channel();
    let generation = {
        let mut inner = client.inner.lock().await;
        inner.generation = inner.generation.saturating_add(1);
        inner.closed = false;
        inner.acknowledged.clear();
        inner.pending_said.clear();
        inner.pending_turn.clear();
        inner.live.clear();
        inner.generation
    };
    *client.write.lock().await = WriteOut { tx: write_tx };
    tokio::spawn(write_loop(writer, write_rx));
    let hello_id = format!("hello:{}", client.instance_id);
    if !send_frame(
        client,
        hello_frame_with_operations(
            &hello_id,
            &client.instance_id,
            revision,
            config_digest,
            client.operations.as_ref(),
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
    tokio::spawn(read_loop(reader, client.clone(), generation));
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
        Ok(SaidOutcome::Disconnected) => {
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
                reason = "channel_closed",
                "hello failed"
            );
            Err(FrameError::Io)
        }
    }
}

async fn reconnect_loop(
    client: Arc<InstanceClient>,
    socket: PathBuf,
    revision: u64,
    config_digest: String,
) {
    let mut backoff = RECONNECT_MIN;
    loop {
        match attach(&client, &socket, revision, &config_digest).await {
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

async fn read_loop(
    mut reader: tokio::net::unix::OwnedReadHalf,
    client: Arc<InstanceClient>,
    generation: u64,
) {
    loop {
        match read_frame(&mut reader).await {
            Ok(bytes) => match parse_frame_bytes(&bytes) {
                Ok(msg) => {
                    if handle_msg(&client, msg, generation).await {
                        break;
                    }
                }
                Err(FrameError::BadRequest) | Err(FrameError::TooLarge) => {
                    close_all(&client, "bad_request", generation).await;
                    break;
                }
                Err(_) => {
                    close_all(&client, "disconnect", generation).await;
                    break;
                }
            },
            Err(FrameError::TooLarge) => {
                close_all(&client, "too_large", generation).await;
                break;
            }
            Err(_) => {
                close_all(&client, "disconnect", generation).await;
                break;
            }
        }
    }
}

