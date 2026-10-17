// D-1083: core→gate `partial` frames are id-less previews; the gate never writes back and never
// closes for them.
use super::*;
use serde_json::json;
use tokio::net::UnixListener;

const ADDRESS: &str = "test:partial";
const BINDING: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const STREAM: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

async fn read(core: &mut UnixStream) -> Value {
    let bytes = tokio::time::timeout(Duration::from_secs(2), read_frame(core))
        .await
        .expect("client frame deadline")
        .expect("client frame");
    serde_json::from_slice(&bytes).unwrap()
}

/// A connected, bound client and the core side of its socket.
async fn bound_client() -> (tempfile::TempDir, Arc<InstanceClient>, UnixStream) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("core.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let client = Arc::new(InstanceClient::blank(
        "instance".into(),
        "author".into(),
        SayPolicy::AcceptToLiveQueue,
        None,
        super::super::wire::RuntimeCapabilities::default(),
        None,
    ));
    let task = {
        let client = client.clone();
        tokio::spawn(async move { attach(&client, &socket, 1, &"0".repeat(64)).await })
    };
    let (mut core, _) = listener.accept().await.unwrap();
    let hello = read(&mut core).await;
    write_json(&mut core, &json!({"id": hello["id"], "m": "ok"}))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
    write_json(
        &mut core,
        &json!({"id":"bind", "m":"bind", "binding_id": BINDING, "address": ADDRESS}),
    )
    .await
    .unwrap();
    assert_eq!(read(&mut core).await, json!({"id":"bind", "m":"ok"}));
    (dir, client, core)
}

async fn partial(core: &mut UnixStream, text: &str) {
    write_json(
        core,
        &json!({"m":"partial", "binding_id": BINDING, "stream_id": STREAM, "text": text}),
    )
    .await
    .unwrap();
}

async fn say(core: &mut UnixStream, id: &str, payload: Value) -> Value {
    write_json(
        core,
        &json!({"id": id, "m":"say", "binding_id": BINDING, "payload": payload}),
    )
    .await
    .unwrap();
    read(core).await
}

/// C11 + C12 (gate-client side): an id-less partial gets zero writes, keeps the connection, and
/// reaches the live queue ahead of the say carrying its stream_id.
#[tokio::test]
async fn partial_is_queued_with_zero_writes_before_its_say() {
    let (_dir, client, mut core) = bound_client().await;

    partial(&mut core, "一文目。").await;
    // The first frame the gate writes after the partial is the say's ok: nothing for the partial.
    let reply = say(
        &mut core,
        "say-1",
        json!({"text": "一文目。二文目。", "stream_id": STREAM}),
    )
    .await;
    assert_eq!(reply, json!({"id":"say-1", "m":"ok"}));
    assert!(client.connection_live().await);

    assert_eq!(
        client.next_live(ADDRESS).await,
        Some(LiveEvent::Partial {
            stream_id: STREAM.into(),
            text: "一文目。".into(),
        })
    );
    assert_eq!(
        client.next_live(ADDRESS).await,
        Some(LiveEvent::Message {
            delivery_id: "say-1".into(),
            text: "一文目。二文目。".into(),
            reply_origin: None,
            stream_id: Some(STREAM.into()),
        })
    );
}

/// C11: a partial into a full live queue is dropped without a close, and a later say is accepted
/// once the queue drains.
#[tokio::test]
async fn partial_into_a_full_live_queue_is_dropped_without_close() {
    let (_dir, client, mut core) = bound_client().await;
    for n in 0..LIVE_QUEUE_CAP {
        let reply = say(&mut core, &format!("fill-{n}"), json!({"text": "x"})).await;
        assert_eq!(reply["m"], "ok");
    }

    partial(&mut core, "落ちる。").await;
    // Frames are handled in order, so this rejected say proves the partial met a full queue.
    let probe = say(&mut core, "probe", json!({"text": "y"})).await;
    assert_eq!((&probe["m"], &probe["code"]), (&json!("err"), &json!("external_rejected")));
    for _ in 0..LIVE_QUEUE_CAP {
        assert!(matches!(
            client.next_live(ADDRESS).await,
            Some(LiveEvent::Message { .. })
        ));
    }
    let reply = say(&mut core, "after", json!({"text": "後の発話。"})).await;

    assert_eq!(reply, json!({"id":"after", "m":"ok"}));
    assert!(client.connection_live().await);
    assert_eq!(
        client.next_live(ADDRESS).await,
        Some(LiveEvent::Message {
            delivery_id: "after".into(),
            text: "後の発話。".into(),
            reply_origin: None,
            stream_id: None,
        })
    );
}
