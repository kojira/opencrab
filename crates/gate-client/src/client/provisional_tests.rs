// D-1085 P5 (gate-client side): provisional requests are acknowledged requests; the partials of a
// provisional run and core's `provisional_result` reach the live queue without a write back.
use super::*;
use serde_json::json;
use tokio::net::UnixListener;

const ADDRESS: &str = "test:provisional";
const PROVISIONAL: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";

async fn read(core: &mut UnixStream) -> Value {
    let bytes = tokio::time::timeout(Duration::from_secs(2), read_frame(core))
        .await
        .expect("client frame deadline")
        .expect("client frame");
    serde_json::from_slice(&bytes).unwrap()
}

async fn bound_client() -> (tempfile::TempDir, Arc<InstanceClient>, UnixStream) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("core.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let client = Arc::new(InstanceClient::blank(
        "instance".into(),
        "agent".into(),
        "author".into(),
        SayPolicy::AcceptToLiveQueue,
        None,
        super::super::wire::RuntimeCapabilities::default(),
        None,
    ));
    let task = {
        let client = client.clone();
        tokio::spawn(async move { attach(&client, &socket).await })
    };
    let (mut core, _) = listener.accept().await.unwrap();
    let hello = read(&mut core).await;
    write_json(&mut core, &json!({"id": hello["id"], "m": "ok"}))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
    (dir, client, core)
}

/// The next live event, or `None` after 2 s (a missing event fails instead of hanging).
async fn next_live(client: &InstanceClient) -> Option<LiveEvent> {
    tokio::time::timeout(Duration::from_secs(2), client.next_live(ADDRESS))
        .await
        .ok()
        .flatten()
}

fn request() -> ProvisionalRequest {
    ProvisionalRequest {
        provisional_id: PROVISIONAL.into(),
        author_id: "u1".into(),
        author_label: None,
        caller: SaidCaller::Owner,
        system_context: Some("ctx".into()),
        agent_prelude: Some("うん、".into()),
        text: "（音声）今日は".into(),
    }
}

/// The provisional frame carries the realtime profile and every promotion input; `ok` resolves
/// it, `err` rejects it.
#[tokio::test]
async fn provisional_and_cancel_are_acknowledged_requests() {
    let (_dir, client, mut core) = bound_client().await;

    let sent = tokio::spawn({
        let client = client.clone();
        async move { client.provisional(ADDRESS, &request()).await }
    });
    let frame = read(&mut core).await;
    assert_eq!(frame["m"], "provisional");
    assert_eq!(frame["session"], ADDRESS);
    assert_eq!(frame["provisional_id"], PROVISIONAL);
    assert_eq!(frame["model_profile"], "realtime");
    assert_eq!(frame["caller"], json!({"role": "owner"}));
    assert_eq!(frame["system_context"], "ctx");
    assert_eq!(frame["agent_prelude"], "うん、");
    assert_eq!(frame["text"], "（音声）今日は");
    write_json(&mut core, &json!({"id": frame["id"], "m": "ok"}))
        .await
        .unwrap();
    assert_eq!(sent.await.unwrap(), Ok(()));

    let cancelled = tokio::spawn({
        let client = client.clone();
        async move { client.provisional_cancel(ADDRESS, PROVISIONAL).await }
    });
    let frame = read(&mut core).await;
    assert_eq!(
        frame,
        json!({"id": frame["id"], "m": "provisional_cancel", "session": ADDRESS,
               "provisional_id": PROVISIONAL})
    );
    write_json(
        &mut core,
        &json!({"id": frame["id"], "m": "err", "code": "bad_request", "detail": null}),
    )
    .await
    .unwrap();
    assert_eq!(
        cancelled.await.unwrap(),
        Err(ProvisionalError::Rejected {
            code: "bad_request".into()
        })
    );
    // D-1099: any key may be used; only a closed connection is not ready.
    client.inner.lock().await.closed = true;
    assert_eq!(
        client.provisional("other", &request()).await,
        Err(ProvisionalError::NotReady)
    );
}

/// Partials of a provisional run keep their id; the result is queued in order.
#[tokio::test]
async fn provisional_partial_and_result_reach_the_live_queue() {
    let (_dir, client, mut core) = bound_client().await;
    write_json(
        &mut core,
        &json!({"m":"partial", "session": ADDRESS, "stream_id": "s1", "text": "はい。",
                "provisional_id": PROVISIONAL}),
    )
    .await
    .unwrap();
    write_json(
        &mut core,
        &json!({"m":"provisional_result", "session": ADDRESS,
                "provisional_id": PROVISIONAL, "promoted": true}),
    )
    .await
    .unwrap();
    assert_eq!(
        next_live(&client).await,
        Some(LiveEvent::Partial {
            stream_id: "s1".into(),
            text: "はい。".into(),
            provisional_id: Some(PROVISIONAL.into()),
        })
    );
    assert_eq!(
        next_live(&client).await,
        Some(LiveEvent::ProvisionalResult {
            provisional_id: PROVISIONAL.into(),
            promoted: true,
        })
    );
    assert!(client.connection_live().await);
}

/// The said carries the provisional id it may promote.
#[test]
fn said_frame_carries_the_provisional_id() {
    let context = SaidContext {
        caller: SaidCaller::Owner,
        start_turn: true,
        system_context: None,
        reply_target: None,
        reference: None,
        live_inbound_scope: super::super::wire::LiveInboundScope::All,
        model_profile: super::super::wire::TurnModelProfile::Realtime,
        agent_prelude: None,
        provisional_id: Some(PROVISIONAL.into()),
    };
    let frame = said_frame_with_context("said:1", ADDRESS, "o", "a", None, Some(&context), "hi", &[]);
    assert_eq!(frame["provisional_id"], PROVISIONAL);
}

/// D-1096: `interrupt` is an acknowledged request carrying `delivered` only when given.
#[tokio::test]
async fn interrupt_is_an_acknowledged_request_with_optional_delivered() {
    let (_dir, client, mut core) = bound_client().await;
    let sent = tokio::spawn({
        let client = client.clone();
        async move { client.interrupt(ADDRESS, Some("一文目。")).await }
    });
    let frame = read(&mut core).await;
    assert_eq!(
        frame,
        json!({"id": frame["id"], "m": "interrupt", "session": ADDRESS,
               "delivered": "一文目。"})
    );
    write_json(&mut core, &json!({"id": frame["id"], "m": "ok"}))
        .await
        .unwrap();
    assert_eq!(sent.await.unwrap(), Ok(()));

    let sent = tokio::spawn({
        let client = client.clone();
        async move { client.interrupt(ADDRESS, None).await }
    });
    let frame = read(&mut core).await;
    assert_eq!(
        frame,
        json!({"id": frame["id"], "m": "interrupt", "session": ADDRESS})
    );
    write_json(&mut core, &json!({"id": frame["id"], "m": "ok"}))
        .await
        .unwrap();
    assert_eq!(sent.await.unwrap(), Ok(()));
    client.inner.lock().await.closed = true;
    assert_eq!(
        client.interrupt("other", None).await,
        Err(ProvisionalError::NotReady)
    );
}
