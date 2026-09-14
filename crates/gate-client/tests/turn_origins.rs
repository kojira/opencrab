use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::client::{InstanceClient, LiveEvent, SaidOutcome};
use opencrab_gate_client::wire::{read_frame, write_json};
use serde_json::{json, Value};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

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

    async fn accept(&mut self, id: &str, seq: i64) {
        self.write(json!({"id": id, "m": "ok", "seq": seq})).await;
    }

    async fn reject(&mut self, id: &str) {
        self.write(json!({"id": id, "m": "ok", "seq": null})).await;
    }

    async fn activity(&mut self, ordinal: u32, state: &str, completed: Option<&str>) {
        self.write(json!({
            "m": "activity",
            "binding_id": BINDING,
            "activity_id": format!("00000000-0000-4000-8000-{ordinal:012}"),
            "state": state,
            "origin": null,
            "completed_target": completed,
        }))
        .await;
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
    assert_eq!(hello["m"], "hello");
    core.write(json!({"id": hello["id"], "m": "ok"})).await;
    let client = connect.await.expect("join connect");
    core.write(json!({
        "id": "bind:1",
        "m": "bind",
        "binding_id": BINDING,
        "address": ADDRESS,
    }))
    .await;
    let bind_ack = core.read().await;
    assert_eq!(bind_ack, json!({"id": "bind:1", "m": "ok"}));
    (client, core)
}

fn post(
    client: &Arc<InstanceClient>,
    origin: &'static str,
) -> JoinHandle<Result<SaidOutcome, opencrab_gate_client::client::PostRefuse>> {
    let client = client.clone();
    tokio::spawn(async move { client.post_said(ADDRESS, origin, origin, &[]).await })
}

async fn read_said(core: &mut MockCore, origin: &str) -> String {
    let frame = core.read().await;
    assert_eq!(frame["m"], "said");
    assert_eq!(frame["origin"], origin);
    frame["id"].as_str().expect("said id").to_string()
}

async fn expect_accepted(
    task: JoinHandle<Result<SaidOutcome, opencrab_gate_client::client::PostRefuse>>,
    seq: i64,
) {
    assert!(matches!(
        task.await.expect("join said").expect("post said"),
        SaidOutcome::Accepted { seq: actual } if actual == seq
    ));
}

async fn next_event(client: &InstanceClient) -> LiveEvent {
    tokio::time::timeout(Duration::from_secs(2), client.next_live(ADDRESS))
        .await
        .expect("live event timeout")
        .expect("live event")
}

async fn expect_activity(client: &InstanceClient, state: &str) {
    assert!(matches!(
        next_event(client).await,
        LiveEvent::Activity { state: actual, .. } if actual == state
    ));
}

async fn expect_no_reply(client: &InstanceClient, origin: &str) {
    assert_eq!(
        next_event(client).await,
        LiveEvent::CompletedNoReply {
            reply_origin: Some(origin.into())
        }
    );
}

async fn expect_no_extra_event(client: &InstanceClient) {
    assert!(
        tokio::time::timeout(Duration::from_millis(50), client.next_live(ADDRESS))
            .await
            .is_err(),
        "unexpected extra live event"
    );
}

#[tokio::test]
async fn visible_a_then_silent_b_uses_b_origin_with_reordered_accepts() {
    let (client, mut core) = setup().await;
    let a = post(&client, "origin-a");
    let a_id = read_said(&mut core, "origin-a").await;
    let b = post(&client, "origin-b");
    let b_id = read_said(&mut core, "origin-b").await;

    core.accept(&b_id, 2).await;
    core.accept(&a_id, 1).await;
    expect_accepted(b, 2).await;
    expect_accepted(a, 1).await;
    core.activity(1, "ended", Some("utterance-a")).await;
    core.activity(2, "started", None).await;
    core.activity(3, "ended", None).await;

    expect_activity(&client, "ended").await;
    assert_eq!(
        next_event(&client).await,
        LiveEvent::Completed {
            target: "utterance-a".into()
        }
    );
    expect_activity(&client, "started").await;
    expect_activity(&client, "ended").await;
    expect_no_reply(&client, "origin-b").await;
    expect_no_extra_event(&client).await;
}

#[tokio::test]
async fn silent_a_then_silent_b_uses_each_origin_once() {
    let (client, mut core) = setup().await;
    let a = post(&client, "origin-a");
    let a_id = read_said(&mut core, "origin-a").await;
    let b = post(&client, "origin-b");
    let b_id = read_said(&mut core, "origin-b").await;

    core.accept(&a_id, 1).await;
    core.accept(&b_id, 2).await;
    expect_accepted(a, 1).await;
    expect_accepted(b, 2).await;
    core.activity(1, "ended", None).await;
    core.activity(2, "started", None).await;
    core.activity(3, "ended", None).await;

    expect_activity(&client, "ended").await;
    expect_no_reply(&client, "origin-a").await;
    expect_activity(&client, "started").await;
    expect_activity(&client, "ended").await;
    expect_no_reply(&client, "origin-b").await;
    expect_no_extra_event(&client).await;
}

#[tokio::test]
async fn rejected_a_does_not_take_accepted_b_origin() {
    let (client, mut core) = setup().await;
    let a = post(&client, "origin-a");
    let a_id = read_said(&mut core, "origin-a").await;
    let b = post(&client, "origin-b");
    let b_id = read_said(&mut core, "origin-b").await;

    core.reject(&a_id).await;
    core.accept(&b_id, 2).await;
    assert!(matches!(
        a.await.expect("join a").expect("post a"),
        SaidOutcome::NotAdmitted
    ));
    expect_accepted(b, 2).await;
    core.activity(1, "started", None).await;
    core.activity(2, "ended", None).await;

    expect_activity(&client, "started").await;
    expect_activity(&client, "ended").await;
    expect_no_reply(&client, "origin-b").await;
    expect_no_extra_event(&client).await;
}

#[tokio::test]
async fn cancelled_rejected_said_releases_its_origin() {
    let (client, mut core) = setup().await;
    let a = post(&client, "origin-a");
    let a_id = read_said(&mut core, "origin-a").await;
    a.abort();
    assert!(a.await.expect_err("cancelled a").is_cancelled());
    core.reject(&a_id).await;

    let b = post(&client, "origin-b");
    let b_id = read_said(&mut core, "origin-b").await;
    core.accept(&b_id, 2).await;
    expect_accepted(b, 2).await;
    core.activity(1, "started", None).await;
    core.activity(2, "ended", None).await;

    expect_activity(&client, "started").await;
    expect_activity(&client, "ended").await;
    expect_no_reply(&client, "origin-b").await;
    expect_no_extra_event(&client).await;
}

#[tokio::test]
async fn cancelled_accepted_said_keeps_core_owned_origin() {
    let (client, mut core) = setup().await;
    let a = post(&client, "origin-a");
    let a_id = read_said(&mut core, "origin-a").await;
    a.abort();
    assert!(a.await.expect_err("cancelled a").is_cancelled());
    core.accept(&a_id, 1).await;

    let b = post(&client, "origin-b");
    let b_id = read_said(&mut core, "origin-b").await;
    core.accept(&b_id, 2).await;
    expect_accepted(b, 2).await;
    core.activity(1, "ended", None).await;
    core.activity(2, "started", None).await;
    core.activity(3, "ended", None).await;

    expect_activity(&client, "ended").await;
    expect_no_reply(&client, "origin-a").await;
    expect_activity(&client, "started").await;
    expect_activity(&client, "ended").await;
    expect_no_reply(&client, "origin-b").await;
    expect_no_extra_event(&client).await;
}

#[tokio::test]
async fn disconnect_clears_outstanding_said_without_no_reply() {
    let (client, mut core) = setup().await;
    let a = post(&client, "origin-a");
    let _a_id = read_said(&mut core, "origin-a").await;
    drop(core);

    assert!(matches!(
        a.await.expect("join a").expect("post a"),
        SaidOutcome::Disconnected
    ));
    assert!(matches!(
        next_event(&client).await,
        LiveEvent::Error { code, .. } if code == "disconnect"
    ));
}
