//! Nostr end-to-end tests over real processes, a real Unix socket, and a synthetic WebSocket relay.
//!
//! Every case starts `opencrab-social-runtime` and `nostr-gate-e2e` from this package. The relay is
//! deliberately tiny (REQ/EVENT/OK/EOSE), but it is a real TCP/WebSocket peer. No live relay, LLM,
//! key, npub, or event id is reused: both Nostr identities are generated for each test.

use futures_util::{SinkExt, StreamExt};
use opencrab_nostr_gate::nostr::{self, Key};
use opencrab_port::{EventKind, GateName};
use opencrab_store::{Store, TurnRecordRow};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

const NOSTR: &str = "nostr";
const TIMEOUT: Duration = Duration::from_secs(15);

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct SyntheticRelay {
    url: String,
    incoming: mpsc::UnboundedSender<Value>,
    subscriptions: mpsc::UnboundedReceiver<Value>,
    outgoing: Arc<Mutex<Vec<Value>>>,
    task: JoinHandle<()>,
}

impl Drop for SyntheticRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SyntheticRelay {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind synthetic relay");
        let address = listener.local_addr().expect("relay local address");
        let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel::<Value>();
        let (subscription_tx, subscription_rx) = mpsc::unbounded_channel::<Value>();
        let outgoing = Arc::new(Mutex::new(Vec::new()));
        let captured = outgoing.clone();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept nostr-gate");
            let ws = tokio_tungstenite::accept_async(tcp)
                .await
                .expect("WebSocket handshake");
            let (mut sink, mut stream) = ws.split();
            let mut subid: Option<String> = None;
            loop {
                tokio::select! {
                    maybe_event = incoming_rx.recv() => {
                        let Some(event) = maybe_event else { break };
                        let id = subid.as_deref().expect("test sent EVENT before gate REQ");
                        sink.send(Message::Text(json!(["EVENT", id, event]).to_string()))
                            .await
                            .expect("send inbound EVENT");
                    }
                    maybe_message = stream.next() => {
                        let Some(message) = maybe_message else { break };
                        match message.expect("read gate relay frame") {
                            Message::Text(text) => {
                                let frame: Value = serde_json::from_str(text.as_ref())
                                    .expect("gate sent JSON relay frame");
                                let items = frame.as_array().expect("relay frame is array");
                                match items.first().and_then(Value::as_str) {
                                    Some("REQ") => {
                                        let id = items.get(1).and_then(Value::as_str)
                                            .expect("REQ subid").to_string();
                                        let filter = items.get(2).cloned().expect("REQ filter");
                                        subid = Some(id.clone());
                                        subscription_tx.send(filter).expect("record REQ");
                                        sink.send(Message::Text(json!(["EOSE", id]).to_string()))
                                            .await
                                            .expect("send EOSE");
                                    }
                                    Some("EVENT") => {
                                        let event = items.get(1).cloned().expect("EVENT body");
                                        let id = event.get("id").and_then(Value::as_str)
                                            .expect("outbound event id").to_string();
                                        captured.lock().unwrap().push(event);
                                        sink.send(Message::Text(json!(["OK", id, true, "stored"]).to_string()))
                                            .await
                                            .expect("send OK");
                                    }
                                    Some("CLOSE") => {}
                                    other => panic!("unexpected relay frame: {other:?}: {frame}"),
                                }
                            }
                            Message::Ping(payload) => {
                                sink.send(Message::Pong(payload)).await.expect("send pong");
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                }
            }
        });
        Self {
            url: format!("ws://{address}"),
            incoming: incoming_tx,
            subscriptions: subscription_rx,
            outgoing,
            task,
        }
    }

    async fn wait_for_subscription(&mut self) -> Value {
        tokio::time::timeout(TIMEOUT, self.subscriptions.recv())
            .await
            .expect("nostr-gate did not send REQ")
            .expect("synthetic relay stopped before REQ")
    }

    fn publish(&self, event: Value) {
        self.incoming
            .send(event)
            .expect("synthetic relay task stopped");
    }

    fn posts(&self) -> Vec<Value> {
        self.outgoing.lock().unwrap().clone()
    }
}

#[derive(Clone, Copy)]
enum Case {
    Reply,
    NoReply,
    PrefixedNoReply,
    ToolThenReply,
    PlaintextToolSettledReply,
}

impl Case {
    fn script(self) -> &'static str {
        match self {
            Case::Reply => "reply",
            Case::NoReply => "no_reply",
            Case::PrefixedNoReply => "prefixed_no_reply",
            Case::ToolThenReply => "tool_then_reply",
            Case::PlaintextToolSettledReply => "plaintext_tool_settled_reply",
        }
    }

    fn expected_reply(self) -> Option<&'static str> {
        match self {
            Case::Reply => Some("mock reply"),
            Case::ToolThenReply => Some("mock reply after tool result"),
            Case::PlaintextToolSettledReply => {
                Some("mock reply after settled plaintext tool result")
            }
            Case::NoReply | Case::PrefixedNoReply => None,
        }
    }
}

fn bin_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .parent()
        .expect("core binary parent")
        .to_path_buf()
}

fn drain_stderr(stderr: std::process::ChildStderr, name: &'static str) {
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[{name}] {line}");
        }
    });
}

fn spawn_gate(socket: &Path, relay_url: &str) -> (Proc, String) {
    let child = Command::new(env!("CARGO_BIN_EXE_nostr-gate-e2e"))
        .arg(socket)
        .env("NOSTR_GATE_RELAY", relay_url)
        .env_remove("NOSTR_GATE_NSEC")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn nostr-gate");
    let mut proc = Proc(child);
    let stderr = proc.0.stderr.take().expect("gate stderr");
    let (npub_tx, npub_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[nostr-gate] {line}");
            if let Some(npub) = line
                .split_whitespace()
                .find(|word| word.starts_with("npub1"))
            {
                let _ = npub_tx.try_send(npub.to_string());
            }
        }
    });
    let npub = npub_rx
        .recv_timeout(TIMEOUT)
        .expect("nostr-gate did not report its generated npub");
    (proc, npub)
}

fn spawn_core(socket: &Path, db: &Path, places: &Path, script: &str) -> Proc {
    let child = Command::new(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .arg(socket)
        .arg(db)
        .env("OPENCRAB_PLACES", places)
        .env("OPENCRAB_LLM_PROVIDER", "mock")
        .env("OPENCRAB_MOCK_LLM_SCRIPT", script)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn opencrab-social-runtime");
    let mut proc = Proc(child);
    drain_stderr(proc.0.stderr.take().expect("core stderr"), "core");
    proc
}

async fn open_store(path: &Path) -> Store {
    let start = Instant::now();
    loop {
        if path.exists() {
            if let Ok(store) = Store::open(path) {
                return store;
            }
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "core database did not become readable"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn find_place(store: &Store, address: &str) -> Option<i64> {
    store.all_open_places().ok()?.into_iter().find(|place| {
        store
            .get_place(*place)
            .ok()
            .flatten()
            .and_then(|row| row.address)
            .as_deref()
            == Some(address)
    })
}

async fn wait_for_turn(store: &Store, place: i64) -> TurnRecordRow {
    let start = Instant::now();
    loop {
        if let Ok(records) = store.turn_records(place) {
            if let Some(record) = records.last() {
                return record.clone();
            }
        }
        assert!(start.elapsed() < TIMEOUT, "turn record was not written");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_turns(store: &Store, place: i64, count: usize) -> Vec<TurnRecordRow> {
    let start = Instant::now();
    loop {
        if let Ok(records) = store.turn_records(place) {
            if records.len() >= count {
                return records;
            }
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "expected {count} turn records were not written"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_post(relay: &SyntheticRelay) -> Value {
    let start = Instant::now();
    loop {
        if let Some(event) = relay.posts().first() {
            return event.clone();
        }
        assert!(start.elapsed() < TIMEOUT, "gate did not publish to relay");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_out_ref(store: &Store, place: i64, expected_text: &str) -> i64 {
    let gate = GateName::new(NOSTR);
    let start = Instant::now();
    loop {
        if let Ok(latest) = store.latest_seq(place) {
            for seq in 1..=latest {
                let event = match store.get_event(place, seq) {
                    Ok(Some(event)) => event,
                    _ => continue,
                };
                if event.kind == EventKind::Spoke
                    && event.content.text.as_deref() == Some(expected_text)
                    && store
                        .external_ref_direction(place, seq, &gate)
                        .ok()
                        .flatten()
                        .as_deref()
                        == Some("out")
                {
                    return seq;
                }
            }
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "outbound external ref was not recorded"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn run_case(case: Case) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut relay = SyntheticRelay::start().await;
    let scratch = tempfile::Builder::new()
        .prefix("ne2e-")
        .tempdir_in(bin_dir())
        .expect("create short E2E directory");
    let socket = scratch.path().join("core.sock");
    let db = scratch.path().join("core.db");
    let places = scratch.path().join("places.json");
    assert!(
        socket.as_os_str().len() < 100,
        "Unix socket path is too long"
    );

    let poster = Key::generate();
    let (gate, gate_npub) = spawn_gate(&socket, &relay.url);
    let address = format!("filter:kind=1&author={}", poster.npub);
    let config = json!({
        "places": [{
            "address": address,
            "gate": NOSTR,
            "name": "synthetic-agent",
            "persona": "You are a synthetic E2E agent.",
            "policy": {
                "immediate": ["mentions_me", "replies_to_me"],
                "immediate_from": "anyone",
                "batch_window_ms": null,
                "unconditional_interval_ms": null
            },
            "identities": [{"gate": NOSTR, "external": gate_npub}]
        }]
    });
    std::fs::write(&places, config.to_string()).expect("write places config");
    let core = spawn_core(&socket, &db, &places, case.script());

    let filter = relay.wait_for_subscription().await;
    assert_eq!(filter["kinds"], json!([1]));
    assert_eq!(filter["authors"], json!([poster.pubkey_hex.clone()]));

    let gate_hex = nostr::npub_to_hex(
        config["places"][0]["identities"][0]["external"]
            .as_str()
            .expect("gate identity"),
    )
    .expect("gate npub to hex");
    let (_incoming_id, incoming) = nostr::build_signed(
        &poster,
        1,
        json!([["p", gate_hex]]),
        &format!("synthetic mention for {}", case.script()),
        nostr::now_secs(),
    );
    relay.publish(incoming);

    let store = open_store(&db).await;
    let place_wait_started = Instant::now();
    let place = loop {
        if let Some(place) = find_place(&store, &address) {
            break place;
        }
        assert!(
            place_wait_started.elapsed() < TIMEOUT,
            "configured Nostr place did not become readable"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let turn = wait_for_turn(&store, place).await;

    match case.expected_reply() {
        Some(expected) => {
            let post = wait_for_post(&relay).await;
            nostr::verify_event(&post).expect("gate produced a valid signed Nostr event");
            assert_eq!(post["kind"], json!(1));
            assert_eq!(post["content"], json!(expected));
            let seq = wait_for_out_ref(&store, place, expected).await;
            assert!(
                store.external_ref_of(place, seq).unwrap().is_some(),
                "outbound Spoke has an external ref"
            );
            if matches!(case, Case::PlaintextToolSettledReply) {
                let turns = wait_for_turns(&store, place, 2).await;
                assert_eq!(turns[0].end_reason, "no_reply");
                assert_eq!(turns[0].iterations, 1);
                assert_eq!(turns[0].tool_lines.as_deref(), Some("nostr-whoami::{}"));
                assert_eq!(turns[1].end_reason, "done");
                assert_eq!(turns[1].iterations, 1);

                let latest = store.latest_seq(place).unwrap();
                let settled = store
                    .read_range(place, 0, latest)
                    .unwrap()
                    .into_iter()
                    .find(|event| event.kind == EventKind::Settled)
                    .expect("plaintext tool writes a settled event");
                assert_eq!(
                    settled.reply_to,
                    Some(1),
                    "settled event retains the originating synthetic mention"
                );
            } else if matches!(case, Case::ToolThenReply) {
                assert_eq!(turn.end_reason, "done");
                assert_eq!(turn.iterations, 2, "tool result causes a second inference");
                assert_eq!(
                    store.context_records(turn.id).unwrap().len(),
                    2,
                    "both inference iterations are observed"
                );
            } else {
                assert_eq!(turn.end_reason, "done");
                assert_eq!(turn.iterations, 1);
            }
        }
        None => {
            assert_eq!(turn.end_reason, "no_reply");
            if matches!(case, Case::PrefixedNoReply) {
                assert_eq!(
                    turn.withheld_text.as_deref(),
                    Some("mock internal reasoning"),
                    "prefixed prose is retained in the turn record"
                );
            } else {
                assert!(
                    turn.withheld_text.is_none(),
                    "bare sentinel has no prose to retain"
                );
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(
                relay.posts().is_empty(),
                "NO_REPLY must publish zero events"
            );
            let latest = store.latest_seq(place).unwrap();
            assert!(
                (1..=latest).all(|seq| {
                    store
                        .get_event(place, seq)
                        .unwrap()
                        .is_none_or(|event| event.kind != EventKind::Spoke)
                }),
                "NO_REPLY must not even confirm a Spoke event"
            );
        }
    }

    drop(store);
    drop(core);
    drop(gate);
    drop(relay);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mention_posts_mock_reply_and_records_outbound_external_ref() {
    run_case(Case::Reply).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_no_reply_posts_nothing_and_records_no_reply_turn() {
    run_case(Case::NoReply).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefixed_no_reply_posts_nothing() {
    run_case(Case::PrefixedNoReply).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_result_drives_second_inference_and_reply() {
    run_case(Case::ToolThenReply).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plaintext_tool_settled_turn_receives_origin_and_replies() {
    run_case(Case::PlaintextToolSettledReply).await;
}
