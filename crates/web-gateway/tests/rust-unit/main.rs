//! rust-unit: frame parser と in-process HTTP。共通 conformance の合格数には算入しない。
//! 旧 `tests/conformance.rs` の in-process 検査を移設した。緩和していない。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use opencrab_web_gateway::v3::client::InstanceClient;
use opencrab_web_gateway::v3::http::{router, HttpState};
use opencrab_web_gateway::v3::wire::{
    config_digest, hello_frame, ok_frame, parse_frame_bytes, read_frame, write_json, CoreMsg,
    FrameError, MAX_FRAME,
};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tower::ServiceExt;
use uuid::Uuid;

mod harvest;
mod invariants;

const INSTANCE: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const BINDING: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const ADDRESS: &str = "web-agent-conv";
const AUTHOR: &str = "owner-1";
const CLIENT_MSG: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

fn client_uuid() -> String {
    CLIENT_MSG.to_string()
}

fn origin() -> String {
    format!("web:{CLIENT_MSG}")
}

struct MockCore {
    incoming: mpsc::UnboundedReceiver<Value>,
    half: std::sync::Arc<tokio::sync::Mutex<Option<tokio::net::unix::OwnedWriteHalf>>>,
}

impl MockCore {
    fn spawn(path: std::path::PathBuf) -> Self {
        let listener = UnixListener::bind(&path).expect("bind mock core");
        let (tx, incoming) = mpsc::unbounded_channel();
        let half = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let half_task = half.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (mut reader, writer) = stream.into_split();
            *half_task.lock().await = Some(writer);
            while let Ok(bytes) = read_frame(&mut reader).await {
                let text = String::from_utf8_lossy(bytes.strip_suffix(b"\n").unwrap_or(&bytes));
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        Self { incoming, half }
    }

    async fn wait_writer(&self) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if self.half.lock().await.is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("accept timeout");
    }

    async fn send(&self, v: &Value) {
        self.wait_writer().await;
        let mut half = self.half.lock().await;
        let w = half.as_mut().expect("writer");
        write_json(w, v).await.expect("mock write");
    }

    async fn recv(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(2), self.incoming.recv())
            .await
            .expect("recv timeout")
            .expect("closed")
    }

    async fn close(self) {
        self.wait_writer().await;
        let mut half = self.half.lock().await;
        if let Some(w) = half.as_mut() {
            let _ = w.shutdown().await;
        }
    }
}

async fn connect_client(sock: &std::path::Path) -> Arc<InstanceClient> {
    InstanceClient::connect(
        sock,
        INSTANCE.to_string(),
        1,
        AUTHOR.to_string(),
        config_digest(AUTHOR),
    )
    .await
    .expect("connect")
}

async fn hello_and_bind(mock: &mut MockCore) {
    let hello = mock.recv().await;
    assert_eq!(hello["m"], "hello");
    assert_eq!(hello["protocol"], 2);
    assert_eq!(hello["instance_id"], INSTANCE);
    mock.send(&json!({"id": hello["id"], "m": "ok"})).await;
    mock.send(&json!({
        "id": format!("bind:{BINDING}"),
        "m": "bind",
        "binding_id": BINDING,
        "address": ADDRESS,
    }))
    .await;
    let ack = mock.recv().await;
    assert_eq!(ack["m"], "ok");
    assert_eq!(ack["id"], format!("bind:{BINDING}"));
}

async fn wait_bound(client: &InstanceClient) {
    for _ in 0..50 {
        if client.binding_for_address(ADDRESS).await.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("bind not acknowledged");
}

fn post_body() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "client_message_id": client_uuid(),
        "text": "hello",
        "attachments": []
    }))
    .unwrap()
}

#[tokio::test]
async fn rust_unit_hello_bind_said_dedup() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("core.sock");
    let mut mock = MockCore::spawn(sock.clone());
    let connect = tokio::spawn(async move { connect_client(&sock).await });
    hello_and_bind(&mut mock).await;
    let client = connect.await.unwrap();
    wait_bound(&client).await;

    let post_a = {
        let client = client.clone();
        tokio::spawn(async move { client.post_said(ADDRESS, &origin(), "hello", &[]).await })
    };
    let said1 = mock.recv().await;
    assert_eq!(said1["m"], "said");
    assert_eq!(said1["origin"], origin());
    assert_eq!(said1["author_id"], AUTHOR);
    assert_eq!(said1["binding_id"], BINDING);
    mock.send(&json!({"id": said1["id"], "m": "ok", "seq": 1}))
        .await;
    match post_a.await.unwrap().unwrap() {
        opencrab_web_gateway::v3::client::SaidOutcome::Accepted { seq } => {
            assert_eq!(seq, 1);
        }
        other => panic!("{other:?}"),
    }

    let post_b = {
        let client = client.clone();
        tokio::spawn(async move { client.post_said(ADDRESS, &origin(), "hello", &[]).await })
    };
    let said2 = mock.recv().await;
    assert_eq!(said2["origin"], origin());
    mock.send(&json!({"id": said2["id"], "m": "ok", "seq": 1}))
        .await;
    match post_b.await.unwrap().unwrap() {
        opencrab_web_gateway::v3::client::SaidOutcome::Accepted { seq } => {
            assert_eq!(seq, 1);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn rust_unit_say_ok_external_rejected_close() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("core.sock");
    let mut mock = MockCore::spawn(sock.clone());
    let connect = tokio::spawn(async move { connect_client(&sock).await });
    hello_and_bind(&mut mock).await;
    let client = connect.await.unwrap();
    wait_bound(&client).await;

    mock.send(&json!({
        "id": "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
        "m": "say",
        "binding_id": BINDING,
        "payload": {"text": "reply", "ignored": true},
    }))
    .await;
    let ok = mock.recv().await;
    assert_eq!(ok["m"], "ok");
    let ev = tokio::time::timeout(Duration::from_secs(2), client.next_live(ADDRESS))
        .await
        .expect("live")
        .expect("event");
    assert_eq!(
        ev,
        opencrab_web_gateway::v3::client::LiveEvent::Message {
            delivery_id: "dddddddd-dddd-4ddd-8ddd-dddddddddddd".into(),
            text: "reply".into(),
            reply_origin: None,
        }
    );

    mock.send(&json!({
        "id": "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee",
        "m": "say",
        "binding_id": BINDING,
        "payload": {"text": ""},
    }))
    .await;
    let rej = mock.recv().await;
    assert_eq!(rej["m"], "err");
    assert_eq!(rej["code"], "external_rejected");

    mock.close().await;
    let gone = tokio::time::timeout(Duration::from_secs(2), client.next_live(ADDRESS))
        .await
        .expect("close live");
    match gone {
        Some(opencrab_web_gateway::v3::client::LiveEvent::Error { code, .. }) => {
            assert_eq!(code, "disconnect");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn rust_unit_activity_started_ended() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("core.sock");
    let mut mock = MockCore::spawn(sock.clone());
    let connect = tokio::spawn(async move { connect_client(&sock).await });
    hello_and_bind(&mut mock).await;
    let client = connect.await.unwrap();
    wait_bound(&client).await;

    let said_fut = {
        let client = client.clone();
        tokio::spawn(async move { client.post_said(ADDRESS, &origin(), "hi", &[]).await })
    };
    let said_msg = mock.recv().await;
    mock.send(&json!({"id": said_msg["id"], "m": "ok", "seq": 3}))
        .await;
    said_fut.await.unwrap().unwrap();

    mock.send(&json!({
        "m": "activity",
        "binding_id": BINDING,
        "activity_id": "ffffffff-ffff-4fff-8fff-ffffffffffff",
        "state": "started",
    }))
    .await;
    let started = tokio::time::timeout(Duration::from_secs(2), client.next_live(ADDRESS))
        .await
        .unwrap()
        .unwrap();
    match started {
        opencrab_web_gateway::v3::client::LiveEvent::Activity { state, .. } => {
            assert_eq!(state, "started");
        }
        other => panic!("{other:?}"),
    }

    mock.send(&json!({
        "m": "activity",
        "binding_id": BINDING,
        "activity_id": "ffffffff-ffff-4fff-8fff-ffffffffffff",
        "state": "ended",
    }))
    .await;
    let ended = tokio::time::timeout(Duration::from_secs(2), client.next_live(ADDRESS))
        .await
        .unwrap()
        .unwrap();
    match ended {
        opencrab_web_gateway::v3::client::LiveEvent::Activity { state, .. } => {
            assert_eq!(state, "ended");
        }
        other => panic!("{other:?}"),
    }
    let none = tokio::time::timeout(Duration::from_secs(2), client.next_live(ADDRESS))
        .await
        .unwrap()
        .unwrap();
    // 即時 said（occupy）が単独で握ったターンなので、発端 origin を運ぶ（Single）。
    assert_eq!(
        none,
        opencrab_web_gateway::v3::client::LiveEvent::CompletedNoReply {
            reply_origin: Some(origin())
        }
    );
}

#[tokio::test]
async fn rust_unit_frame_too_large_and_duplicate_close() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("core.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let connect = tokio::spawn({
        let sock = sock.clone();
        async move { connect_client(&sock).await }
    });
    let (stream, _) = listener.accept().await.unwrap();
    let (mut reader, mut writer) = stream.into_split();
    let hello_bytes = read_frame(&mut reader).await.unwrap();
    let hello = parse_frame_bytes(&hello_bytes).unwrap();
    let hello_id = match hello {
        CoreMsg::Reverse { id, m } if m == "hello" => id.expect("id"),
        _ => {
            let v: Value =
                serde_json::from_slice(hello_bytes.strip_suffix(b"\n").unwrap()).unwrap();
            v["id"].as_str().unwrap().to_string()
        }
    };
    write_json(&mut writer, &json!({"id": hello_id, "m": "ok"}))
        .await
        .unwrap();
    write_json(
        &mut writer,
        &json!({
            "id": format!("bind:{BINDING}"),
            "m": "bind",
            "binding_id": BINDING,
            "address": ADDRESS,
        }),
    )
    .await
    .unwrap();
    let _ = read_frame(&mut reader).await;
    let client = connect.await.unwrap();
    wait_bound(&client).await;

    let mut huge = vec![b'x'; MAX_FRAME];
    huge.push(b'\n');
    writer.write_all(&huge).await.unwrap();
    let ev = tokio::time::timeout(Duration::from_secs(2), client.next_live(ADDRESS))
        .await
        .unwrap();
    match ev {
        Some(opencrab_web_gateway::v3::client::LiveEvent::Error { code, .. }) => {
            assert_eq!(code, "too_large");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn rust_unit_http_post_202_not_admitted_busy_and_old_routes_404() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("core.sock");
    let mut mock = MockCore::spawn(sock.clone());
    let connect = tokio::spawn(async move { connect_client(&sock).await });
    hello_and_bind(&mut mock).await;
    let client = connect.await.unwrap();
    wait_bound(&client).await;
    let app = router(HttpState {
        instances: vec![client.clone()],
    });

    let req_null = Request::builder()
        .method("POST")
        .uri(format!("/api/web-conversations/{ADDRESS}/messages"))
        .header("content-type", "application/json")
        .body(Body::from(post_body()))
        .unwrap();
    let pending_null = tokio::spawn(app.clone().oneshot(req_null));
    let said_null = mock.recv().await;
    mock.send(&json!({"id": said_null["id"], "m": "ok", "seq": null}))
        .await;
    let res_null = pending_null.await.unwrap().unwrap();
    assert_eq!(res_null.status(), StatusCode::FORBIDDEN);
    let body_null = res_null.into_body().collect().await.unwrap().to_bytes();
    let vn: Value = serde_json::from_slice(&body_null).unwrap();
    assert_eq!(vn["state"], "not_admitted");

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/web-conversations/{ADDRESS}/messages"))
        .header("content-type", "application/json")
        .body(Body::from(post_body()))
        .unwrap();
    let pending = tokio::spawn(app.clone().oneshot(req));
    let said = mock.recv().await;
    assert_eq!(said["m"], "said");
    assert_eq!(said["origin"], origin());
    mock.send(&json!({"id": said["id"], "m": "ok", "seq": 7}))
        .await;
    let res = pending.await.unwrap().unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["state"], "accepted");
    assert_eq!(v["seq"], 7);
    assert_eq!(v["origin"], origin());
    assert_eq!(v["client_message_id"], CLIENT_MSG);

    let during = {
        let app = app.clone();
        tokio::spawn(async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/web-conversations/{ADDRESS}/messages"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "client_message_id": "ffffffff-ffff-4fff-8fff-ffffffffffff",
                            "text": "during-turn",
                            "attachments": []
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
        })
    };
    let said_during = mock.recv().await;
    assert_eq!(said_during["m"], "said");
    assert_eq!(
        said_during["origin"],
        "web:ffffffff-ffff-4fff-8fff-ffffffffffff"
    );
    mock.send(&json!({"id": said_during["id"], "m": "ok", "seq": 8}))
        .await;
    let res_during = during.await.unwrap().unwrap();
    assert_eq!(res_during.status(), StatusCode::ACCEPTED);
    let body_during = res_during.into_body().collect().await.unwrap().to_bytes();
    let vd: Value = serde_json::from_slice(&body_during).unwrap();
    assert_eq!(vd["state"], "accepted");
    assert_eq!(vd["seq"], 8);

    for uri in ["/rooms/x/messages", "/chat"] {
        let get = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::NOT_FOUND, "{uri}");
    }
    let post_rooms = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rooms/x/messages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(post_rooms.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rust_unit_disconnect_and_unacked_503() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("core.sock");
    let mut mock = MockCore::spawn(sock.clone());
    let connect = tokio::spawn(async move { connect_client(&sock).await });
    let hello = mock.recv().await;
    mock.send(&json!({"id": hello["id"], "m": "ok"})).await;
    let client = connect.await.unwrap();
    let app = router(HttpState {
        instances: vec![client.clone()],
    });
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/web-conversations/{ADDRESS}/messages"))
                .header("content-type", "application/json")
                .body(Body::from(post_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);

    mock.send(&json!({
        "id": format!("bind:{BINDING}"),
        "m": "bind",
        "binding_id": BINDING,
        "address": ADDRESS,
    }))
    .await;
    let _ = mock.recv().await;
    wait_bound(&client).await;
    mock.close().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/web-conversations/{ADDRESS}/messages"))
                .header("content-type", "application/json")
                .body(Body::from(post_body()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn rust_unit_bind_same_address_different_binding_closes() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("core.sock");
    let mut mock = MockCore::spawn(sock.clone());
    let connect = tokio::spawn(async move { connect_client(&sock).await });
    hello_and_bind(&mut mock).await;
    let client = connect.await.unwrap();
    wait_bound(&client).await;
    mock.send(&json!({
        "id": "bind:other",
        "m": "bind",
        "binding_id": "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
        "address": ADDRESS,
    }))
    .await;
    let ev = tokio::time::timeout(Duration::from_secs(2), client.next_live(ADDRESS))
        .await
        .expect("live")
        .expect("event");
    match ev {
        opencrab_web_gateway::v3::client::LiveEvent::Error { code, .. } => {
            assert_eq!(code, "binding_conflict");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn rust_unit_frame_grammar_unit() {
    let _ = hello_frame("h", INSTANCE, 1, &config_digest(AUTHOR));
    let _ = ok_frame("x");
    assert!(matches!(
        parse_frame_bytes(br#"{"m":1}"#).unwrap_or(CoreMsg::Invalid {
            id: None,
            code: "bad_request",
            m: String::new(),
        }),
        CoreMsg::Invalid { .. }
    ));
    let dup = parse_frame_bytes(br#"{"id":"1","m":"ok","id":"2"}"#);
    assert!(matches!(dup, Err(FrameError::BadRequest)));
    let _ = Uuid::parse_str(CLIENT_MSG).unwrap();
}
