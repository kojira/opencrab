use super::*;
use serde_json::json;
use tokio::net::UnixListener;
use tokio::sync::Semaphore;

const ADDRESS: &str = "test:transport";
const BINDING: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

struct HeldHandler {
    started: mpsc::UnboundedSender<String>,
    dropped: mpsc::UnboundedSender<()>,
    release: Semaphore,
    subsequent_stages: AtomicU64,
}

struct DropSignal(mpsc::UnboundedSender<()>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

#[async_trait::async_trait]
impl InvokeHandler for HeldHandler {
    async fn handle(&self, id: &str, _: &str, _: &str, _: &Value) -> InvokeOutcome {
        let _guard = DropSignal(self.dropped.clone());
        self.started.send(id.into()).unwrap();
        if id == "first" {
            self.release.acquire().await.unwrap().forget();
        }
        self.subsequent_stages.fetch_add(1, Ordering::SeqCst);
        InvokeOutcome::Ok(json!(null))
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    listener: UnixListener,
    client: Arc<InstanceClient>,
    handler: Arc<HeldHandler>,
    started: mpsc::UnboundedReceiver<String>,
    dropped: mpsc::UnboundedReceiver<()>,
    core: UnixStream,
}

async fn read(core: &mut UnixStream) -> Value {
    let bytes = tokio::time::timeout(Duration::from_secs(2), read_frame(core))
        .await
        .expect("client frame deadline")
        .expect("client frame");
    serde_json::from_slice(&bytes).unwrap()
}

async fn connect(
    client: Arc<InstanceClient>,
    socket: PathBuf,
    listener: &UnixListener,
) -> UnixStream {
    let task = tokio::spawn(async move { attach(&client, &socket, 1, &"0".repeat(64)).await });
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
    core
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("core.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (started_tx, started) = mpsc::unbounded_channel();
        let (dropped_tx, dropped) = mpsc::unbounded_channel();
        let handler = Arc::new(HeldHandler {
            started: started_tx,
            dropped: dropped_tx,
            release: Semaphore::new(0),
            subsequent_stages: AtomicU64::new(0),
        });
        let operations = json!([{
            "name":"deliver", "description":"test", "input_schema":{"type":"object"},
            "output_schema":null, "callback_schema":null,
            "authorization":{"allowed_callers":["owner"]}, "dispatch":"background",
            "sub_engine":"not_exposed", "sharing":"agent_bound", "effect":"utterance"
        }]);
        let client = Arc::new(InstanceClient::blank(
            "instance".into(),
            "author".into(),
            SayPolicy::AcceptToLiveQueue,
            Some(operations),
            super::super::wire::RuntimeCapabilities::default(),
            Some(handler.clone()),
        ));
        let core = connect(client.clone(), socket.clone(), &listener).await;
        Self {
            _dir: dir,
            socket,
            listener,
            client,
            handler,
            started,
            dropped,
            core,
        }
    }

    async fn invoke(&mut self, id: &str) {
        let digest = crate::wire::runtime_declaration_digest_from_value(
            self.client.operations.as_ref(),
            self.client.runtime_capabilities,
        )
        .unwrap();
        write_json(
            &mut self.core,
            &json!({"m":"invoke", "id":id, "binding_id":BINDING,
            "declaration_digest":digest, "operation":"deliver", "dispatch":"background",
            "effect":"utterance", "invocation_protocol":1, "payload":{}}),
        )
        .await
        .unwrap();
    }

    async fn start_held(&mut self) {
        self.invoke("first").await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), self.started.recv())
                .await
                .unwrap()
                .unwrap(),
            "first"
        );
    }

    async fn activity(&mut self, state: &str) {
        write_json(
            &mut self.core,
            &json!({"m":"activity", "binding_id":BINDING,
            "activity_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "state":state}),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn held_invoke_receives_said_ack_and_preserves_invoke_activity_order() {
    let mut f = Fixture::new().await;
    f.activity("started").await;
    assert!(
        matches!(f.client.next_live(ADDRESS).await, Some(LiveEvent::Activity {state, ..}) if state == "started")
    );
    f.start_held().await;
    f.invoke("second").await;
    f.activity("ended").await;
    let said = {
        let client = f.client.clone();
        tokio::spawn(async move { client.post_said(ADDRESS, "origin", "text", &[]).await })
    };
    let frame = read(&mut f.core).await;
    assert_eq!(frame["m"], "said");
    write_json(&mut f.core, &json!({"m":"ok", "id":frame["id"], "seq":7}))
        .await
        .unwrap();
    // Use the actual existing ACK deadline, not an inflated test timeout.
    assert!(
        matches!(
            said.await.unwrap().unwrap(),
            SaidOutcome::Accepted { seq: 7 }
        ),
        "supplied ACK must arrive while the invocation is still held"
    );
    assert!(f.client.connection_live().await);
    assert!(
        f.started.try_recv().is_err(),
        "second invoke overtook first"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), f.client.next_live(ADDRESS))
            .await
            .is_err(),
        "activity overtook invoke"
    );
    f.handler.release.add_permits(1);
    assert_eq!(read(&mut f.core).await["id"], "first");
    assert_eq!(read(&mut f.core).await["id"], "second");
    assert_eq!(f.started.recv().await.unwrap(), "second");
    assert!(
        matches!(f.client.next_live(ADDRESS).await, Some(LiveEvent::Activity {state, ..}) if state == "ended")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), f.client.next_live(ADDRESS))
            .await
            .is_err(),
        "utterance was falsely classified silent"
    );
}

#[tokio::test]
async fn disconnect_cancels_handler_and_fences_replacement_generation() {
    cancellation_case(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_close_cancels_handler_and_fences_replacement_generation() {
    cancellation_case(true).await;
}

async fn cancellation_case(explicit: bool) {
    let mut f = Fixture::new().await;
    f.start_held().await;
    f.invoke("discarded").await;
    f.activity("ended").await;
    if explicit {
        close_all(&f.client, "test_close", 1).await;
    } else {
        f.core.shutdown().await.unwrap();
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(2), f.dropped.recv())
            .await
            .is_ok(),
        "closing a generation must drop the active handler without releasing it"
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), read_frame(&mut f.core))
            .await
            .unwrap()
            .is_err(),
        "retired writer must close"
    );
    let mut replacement = connect(f.client.clone(), f.socket.clone(), &f.listener).await;
    f.handler.release.add_permits(1);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), read_frame(&mut replacement))
            .await
            .is_err(),
        "old result escaped into replacement"
    );
    assert_eq!(
        f.handler.subsequent_stages.load(Ordering::SeqCst),
        0,
        "cancelled work must not start its next external stage"
    );
    assert!(
        f.started.try_recv().is_err(),
        "queued work escaped cancellation"
    );
    // A stale response must not consume new pending requests or close the new generation.
    handle_response(
        &f.client,
        WireResponse {
            id: "stale".into(),
            ok: true,
            seq: None,
            result: None,
            code: None,
            message: None,
            detail: None,
        },
        1,
    )
    .await;
    close_all(&f.client, "stale_close", 1).await;
    assert!(f.client.connection_live().await);
}

#[tokio::test]
async fn fatal_frame_cancels_held_handler() {
    let mut f = Fixture::new().await;
    f.start_held().await;
    // Empty frames are fatal framing errors, independent of invocation completion.
    f.core.write_all(b"\n").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), f.dropped.recv())
            .await
            .is_ok()
    );
    assert!(!f.client.connection_live().await);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), read_frame(&mut f.core))
            .await
            .unwrap()
            .is_err()
    );
    f.handler.release.add_permits(1);
    assert_eq!(f.handler.subsequent_stages.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pending_frame_overflow_closes_and_discards_work() {
    let mut f = Fixture::new().await;
    f.start_held().await;
    let digest = crate::wire::runtime_declaration_digest_from_value(
        f.client.operations.as_ref(),
        f.client.runtime_capabilities,
    )
    .unwrap();
    let (mut reader, mut writer) = f.core.into_split();
    let writes = tokio::spawn(async move {
        for i in 0..32 {
            write_json(
                &mut writer,
                &json!({"m":"invoke", "id":format!("queued-{i}"),
                "binding_id":BINDING, "declaration_digest":digest, "operation":"deliver",
                "dispatch":"background", "effect":"utterance", "invocation_protocol":1,
                "payload":{}}),
            )
            .await
            .unwrap();
        }
        writer
    });
    let mut writer = tokio::time::timeout(Duration::from_secs(2), writes)
        .await
        .unwrap()
        .unwrap();
    let said = {
        let client = f.client.clone();
        tokio::spawn(async move { client.post_said(ADDRESS, "origin", "text", &[]).await })
    };
    let frame = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut reader))
        .await
        .unwrap()
        .unwrap();
    let frame: Value = serde_json::from_slice(&frame).unwrap();
    write_json(&mut writer, &json!({"m":"ok", "id":frame["id"], "seq":1}))
        .await
        .unwrap();
    assert!(
        matches!(
            said.await.unwrap().unwrap(),
            SaidOutcome::Accepted { seq: 1 }
        ),
        "exactly 32 queued frames must still allow response processing"
    );
    assert!(f.client.connection_live().await);
    // The next ordinary frame exceeds the bound; responses did not consume queue capacity.
    write_json(
        &mut writer,
        &json!({"m":"activity", "binding_id":BINDING,
        "activity_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "state":"ended"}),
    )
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), f.dropped.recv())
            .await
            .is_ok(),
        "33 pending ordinary frames must retire the generation"
    );
    assert!(!f.client.connection_live().await);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), read_frame(&mut reader))
            .await
            .unwrap()
            .is_err()
    );
    f.handler.release.add_permits(1);
    assert_eq!(f.handler.subsequent_stages.load(Ordering::SeqCst), 0);
    assert!(f.started.try_recv().is_err());
}
