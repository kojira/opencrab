use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::client::{InstanceClient, LiveEvent, SaidOutcome};
use opencrab_gate_client::wire::{read_frame, write_json};
use serde_json::{json, Value};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixListener;

const ADDRESS: &str = "discord:test";
const BINDING: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

struct MockCore {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    socket: PathBuf,
}

impl Drop for MockCore {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl MockCore {
    async fn read(&mut self) -> Value {
        let bytes = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut self.reader))
            .await
            .expect("client frame timeout")
            .expect("client frame");
        serde_json::from_slice(&bytes).expect("client JSON")
    }

    async fn write(&mut self, value: Value) {
        write_json(&mut self.writer, &value)
            .await
            .expect("write core frame");
    }

    async fn accept_next_said(&mut self, origin: &str, seq: i64) {
        let frame = self.read().await;
        assert_eq!(frame["m"], "said");
        assert_eq!(frame["origin"], origin);
        self.write(json!({"id": frame["id"], "m": "ok", "seq": seq}))
            .await;
    }

    async fn activity(&mut self, state: &str, extra: Value) {
        let mut frame = json!({
            "m": "activity",
            "binding_id": BINDING,
            "activity_id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            "state": state,
        });
        if let Value::Object(extra) = extra {
            frame.as_object_mut().unwrap().extend(extra);
        }
        self.write(frame).await;
    }
}

async fn setup() -> (Arc<InstanceClient>, MockCore) {
    let socket = PathBuf::from(format!("/tmp/oc-{}.sock", uuid::Uuid::new_v4()));
    let listener = UnixListener::bind(&socket).expect("bind mock core");
    let connect_socket = socket.clone();
    let connect = tokio::spawn(async move {
        InstanceClient::connect(
            &connect_socket,
            "11111111-1111-4111-8111-111111111111".into(),
            1,
            "author".into(),
            "0".repeat(64),
        )
        .await
        .expect("connect client")
    });
    let (stream, _) = listener.accept().await.expect("accept client");
    let (reader, writer) = stream.into_split();
    let mut core = MockCore {
        reader,
        writer,
        socket,
    };
    let hello = core.read().await;
    core.write(json!({"id": hello["id"], "m": "ok"})).await;
    let client = connect.await.expect("join connect");
    core.write(json!({
        "id": "bind:1",
        "m": "bind",
        "binding_id": BINDING,
        "address": ADDRESS,
    }))
    .await;
    assert_eq!(core.read().await, json!({"id": "bind:1", "m": "ok"}));
    (client, core)
}

async fn post_and_accept(
    client: &Arc<InstanceClient>,
    core: &mut MockCore,
    origin: &'static str,
    seq: i64,
) {
    let task = {
        let client = client.clone();
        tokio::spawn(async move { client.post_said(ADDRESS, origin, origin, &[]).await })
    };
    core.accept_next_said(origin, seq).await;
    assert!(matches!(
        task.await.expect("said task").expect("post said"),
        SaidOutcome::Accepted { seq: actual } if actual == seq
    ));
}

async fn event(client: &InstanceClient) -> LiveEvent {
    tokio::time::timeout(Duration::from_secs(2), client.next_live(ADDRESS))
        .await
        .expect("event timeout")
        .expect("live event")
}

async fn no_extra(client: &InstanceClient) {
    assert!(
        tokio::time::timeout(Duration::from_millis(75), client.next_live(ADDRESS))
            .await
            .is_err(),
        "unexpected extra event"
    );
}

#[tokio::test]
async fn one_execution_two_inbounds_reports_only_authoritative_silent_origin() {
    let (client, mut core) = setup().await;
    post_and_accept(&client, &mut core, "origin-a", 1).await;
    post_and_accept(&client, &mut core, "origin-b", 2).await;
    core.activity("started", json!({})).await;
    core.activity("ended", json!({"silent_origins":["origin-b"]}))
        .await;

    assert!(
        matches!(event(&client).await, LiveEvent::Activity { state, .. } if state == "started")
    );
    assert!(matches!(event(&client).await, LiveEvent::Activity { state, .. } if state == "ended"));
    assert_eq!(
        event(&client).await,
        LiveEvent::CompletedNoReply {
            reply_origin: Some("origin-b".into())
        }
    );
    no_extra(&client).await;
}

#[tokio::test]
async fn authoritative_empty_suppresses_legacy_inference() {
    let (client, mut core) = setup().await;
    post_and_accept(&client, &mut core, "origin-a", 1).await;
    core.activity("started", json!({})).await;
    core.activity("ended", json!({"silent_origins":[]})).await;

    assert!(
        matches!(event(&client).await, LiveEvent::Activity { state, .. } if state == "started")
    );
    assert!(matches!(event(&client).await, LiveEvent::Activity { state, .. } if state == "ended"));
    no_extra(&client).await;
}

#[tokio::test]
async fn absent_field_keeps_legacy_standalone_fallback() {
    let (client, mut core) = setup().await;
    post_and_accept(&client, &mut core, "origin-a", 1).await;
    core.activity("started", json!({})).await;
    core.activity("ended", json!({})).await;

    assert!(
        matches!(event(&client).await, LiveEvent::Activity { state, .. } if state == "started")
    );
    assert!(matches!(event(&client).await, LiveEvent::Activity { state, .. } if state == "ended"));
    assert_eq!(
        event(&client).await,
        LiveEvent::CompletedNoReply {
            reply_origin: Some("origin-a".into())
        }
    );
}

#[tokio::test]
async fn completed_target_and_silent_origins_coexist_in_stable_order() {
    let (client, mut core) = setup().await;
    post_and_accept(&client, &mut core, "origin-a", 1).await;
    post_and_accept(&client, &mut core, "origin-b", 2).await;
    core.activity("started", json!({})).await;
    core.activity(
        "ended",
        json!({
            "completed_target":"cccccccc-cccc-4ccc-8ccc-cccccccccccc",
            "silent_origins":["origin-b","origin-b"]
        }),
    )
    .await;

    assert!(
        matches!(event(&client).await, LiveEvent::Activity { state, .. } if state == "started")
    );
    assert!(matches!(event(&client).await, LiveEvent::Activity { state, .. } if state == "ended"));
    assert_eq!(
        event(&client).await,
        LiveEvent::Completed {
            target: "cccccccc-cccc-4ccc-8ccc-cccccccccccc".into()
        }
    );
    assert_eq!(
        event(&client).await,
        LiveEvent::CompletedNoReply {
            reply_origin: Some("origin-b".into())
        }
    );
    no_extra(&client).await;
}

#[tokio::test]
async fn malformed_silent_origins_is_rejected_without_legacy_fallback() {
    let (client, mut core) = setup().await;
    post_and_accept(&client, &mut core, "origin-a", 1).await;
    core.activity("started", json!({})).await;
    core.activity("ended", json!({"id":"bad:1","silent_origins":[7]}))
        .await;
    assert_eq!(
        core.read().await,
        json!({"id":"bad:1","m":"err","code":"bad_request","detail":null})
    );

    assert!(
        matches!(event(&client).await, LiveEvent::Activity { state, .. } if state == "started")
    );
    no_extra(&client).await;
}
