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
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
use tokio::net::TcpListener;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

const NOSTR: &str = "nostr";
const TIMEOUT: Duration = Duration::from_secs(15);
const QUIESCENCE: Duration = Duration::from_millis(300);

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct E2eScratch {
    path: PathBuf,
    _temporary: Option<tempfile::TempDir>,
}

impl E2eScratch {
    fn new() -> Self {
        let temporary = tempfile::Builder::new()
            .prefix("ne2e-")
            .tempdir_in(bin_dir())
            .expect("create short E2E directory");
        let path = temporary.path().to_path_buf();
        if std::env::var("OPENCRAB_E2E_KEEP_ARTIFACTS").as_deref() == Ok("1") {
            let path = temporary.keep();
            eprintln!("[nostr-e2e] preserving artifacts at {}", path.display());
            Self {
                path,
                _temporary: None,
            }
        } else {
            Self {
                path,
                _temporary: Some(temporary),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
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
    async fn start(log_path: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind synthetic relay");
        let address = listener.local_addr().expect("relay local address");
        let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel::<Value>();
        let (subscription_tx, subscription_rx) = mpsc::unbounded_channel::<Value>();
        let outgoing = Arc::new(Mutex::new(Vec::new()));
        let captured = outgoing.clone();
        let mut log = std::fs::File::create(log_path)
            .unwrap_or_else(|error| panic!("create relay log {}: {error}", log_path.display()));
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept nostr-gate");
            writeln!(log, "accepted websocket peer").expect("write relay log");
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
                        writeln!(log, "relay -> gate: {}", json!(["EVENT", id, &event])).expect("write relay log");
                        sink.send(Message::Text(json!(["EVENT", id, event]).to_string()))
                            .await
                            .expect("send inbound EVENT");
                    }
                    maybe_message = stream.next() => {
                        let Some(message) = maybe_message else { break };
                        match message.expect("read gate relay frame") {
                            Message::Text(text) => {
                                writeln!(log, "gate -> relay: {text}").expect("write relay log");
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

    async fn wait_for_hello_ready_subscription(&mut self) -> Value {
        tokio::time::timeout(TIMEOUT, self.subscriptions.recv())
            .await
            .expect("nostr-gate did not complete core hello and send REQ")
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
    History,
    NoReply,
    PrefixedNoReply,
    ToolThenReply,
    PlaintextToolSettledReply,
}

impl Case {
    fn script(self) -> &'static str {
        match self {
            Case::Reply => "reply",
            Case::History => "history",
            Case::NoReply => "no_reply",
            Case::PrefixedNoReply => "prefixed_no_reply",
            Case::ToolThenReply => "tool_then_reply",
            Case::PlaintextToolSettledReply => "plaintext_tool_settled_reply",
        }
    }

    fn expected_reply(self) -> Option<&'static str> {
        match self {
            Case::Reply => Some("mock reply"),
            Case::History => Some("mock remembered synthetic history seed"),
            Case::ToolThenReply => Some("mock reply after tool result"),
            Case::PlaintextToolSettledReply => {
                Some("mock reply after settled plaintext tool result")
            }
            Case::NoReply | Case::PrefixedNoReply => None,
        }
    }

    fn first_input(self) -> String {
        match self {
            Case::History => "synthetic history seed".to_string(),
            other => format!("synthetic mention for {}", other.script()),
        }
    }
}

fn bin_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .parent()
        .expect("core binary parent")
        .to_path_buf()
}

fn drain_stderr(stderr: std::process::ChildStderr, name: &'static str, log_path: PathBuf) {
    std::thread::spawn(move || {
        let mut log = std::fs::File::create(&log_path)
            .unwrap_or_else(|error| panic!("create child log {}: {error}", log_path.display()));
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[{name}] {line}");
            writeln!(log, "{line}").expect("write child log");
        }
    });
}

fn spawn_gate(socket: &Path, relay_url: &str, log_path: &Path) -> (Proc, String) {
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
    let log_path = log_path.to_path_buf();
    let (npub_tx, npub_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut log = std::fs::File::create(&log_path)
            .unwrap_or_else(|error| panic!("create gate log {}: {error}", log_path.display()));
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[nostr-gate] {line}");
            writeln!(log, "{line}").expect("write gate log");
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

fn spawn_core(
    socket: &Path,
    db: &Path,
    places: &Path,
    script: &str,
    clock_socket: Option<&Path>,
    log_path: &Path,
) -> Proc {
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencrab-social-runtime"));
    command
        .arg(socket)
        .arg(db)
        .env("OPENCRAB_PLACES", places)
        .env("OPENCRAB_LLM_PROVIDER", "mock")
        .env("OPENCRAB_MOCK_LLM_SCRIPT", script)
        .env_remove("OPENCRAB_TEST_CLOCK_SOCKET")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(clock_socket) = clock_socket {
        command.env("OPENCRAB_TEST_CLOCK_SOCKET", clock_socket);
    }
    let child = command.spawn().expect("spawn opencrab-social-runtime");
    let mut proc = Proc(child);
    drain_stderr(
        proc.0.stderr.take().expect("core stderr"),
        "core",
        log_path.to_path_buf(),
    );
    proc
}

async fn open_store(path: &Path) -> Store {
    let start = Instant::now();
    loop {
        if path.exists() {
            // The child core owns startup recovery. This process is only a live observer: using
            // Store::open here would treat the active child epoch as stale and close it.
            if let Ok(store) = Store::open_read_only(path) {
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

async fn wait_for_quiescent_posts(
    relay: &SyntheticRelay,
    store: &Store,
    place: i64,
    expected: usize,
) -> Vec<Value> {
    let start = Instant::now();
    let mut stable_since = None;
    loop {
        let posts = relay.posts();
        assert!(
            posts.len() <= expected,
            "relay published more than the exact expected count: expected={expected}, posts={posts:?}"
        );
        if posts.len() == expected {
            let since = stable_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= QUIESCENCE {
                return posts;
            }
        } else {
            stable_since = None;
        }
        assert!(
            !relay.task.is_finished(),
            "synthetic relay stopped before quiescence; expected={expected}; turns={:?}; events={:?}",
            store.turn_records(place),
            store.read_range(place, 0, store.latest_seq(place).unwrap_or(0))
        );
        assert!(
            start.elapsed() < TIMEOUT,
            "relay did not reach exact count {expected} and quiesce; turns={:?}; events={:?}",
            store.turn_records(place),
            store.read_range(place, 0, store.latest_seq(place).unwrap_or(0))
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_event_text(store: &Store, place: i64, expected: &str) {
    let start = Instant::now();
    loop {
        let latest = store.latest_seq(place).unwrap_or(0);
        if store
            .read_range(place, 0, latest)
            .unwrap_or_default()
            .iter()
            .any(|event| event.content.text.as_deref() == Some(expected))
        {
            return;
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "event was not accepted by core: {expected}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

struct ClockControl {
    lines: tokio::io::Lines<TokioBufReader<tokio::net::unix::OwnedReadHalf>>,
    write: tokio::net::unix::OwnedWriteHalf,
}

impl ClockControl {
    async fn connect(path: &Path) -> Self {
        let start = Instant::now();
        loop {
            match UnixStream::connect(path).await {
                Ok(stream) => {
                    let (read, write) = stream.into_split();
                    return Self {
                        lines: TokioBufReader::new(read).lines(),
                        write,
                    };
                }
                Err(error) => {
                    assert!(
                        start.elapsed() < TIMEOUT,
                        "test clock socket did not become ready: {error}"
                    );
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        }
    }

    async fn advance(&mut self, duration: Duration) {
        let millis = u64::try_from(duration.as_millis()).expect("test duration fits u64 millis");
        self.write
            .write_all(format!("advance_ms={millis}\n").as_bytes())
            .await
            .expect("send fake-clock advance");
        let response = self
            .lines
            .next_line()
            .await
            .expect("read fake-clock response")
            .expect("fake-clock server closed");
        assert_eq!(response, format!("advanced_ms={millis}"));
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
    let scratch = E2eScratch::new();
    let mut relay = SyntheticRelay::start(&scratch.path().join("relay.log")).await;
    let socket = scratch.path().join("core.sock");
    let db = scratch.path().join("core.db");
    let places = scratch.path().join("places.json");
    assert!(
        socket.as_os_str().len() < 100,
        "Unix socket path is too long"
    );

    let poster = Key::generate();
    let (gate, gate_npub) =
        spawn_gate(&socket, &relay.url, &scratch.path().join("gate.stderr.log"));
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
    let core = spawn_core(
        &socket,
        &db,
        &places,
        case.script(),
        None,
        &scratch.path().join("core.stderr.log"),
    );

    // nostr-gate does not open a relay subscription until core has accepted its checked hello and
    // the gate has accepted the initial bind. REQ is therefore the synchronization boundary after
    // which the synthetic relay may inject an EVENT.
    let filter = relay.wait_for_hello_ready_subscription().await;
    assert_eq!(filter["kinds"], json!([1]));
    assert_eq!(filter["authors"], json!([poster.pubkey_hex.clone()]));

    let gate_hex = nostr::npub_to_hex(
        config["places"][0]["identities"][0]["external"]
            .as_str()
            .expect("gate identity"),
    )
    .expect("gate npub to hex");

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
    let (_incoming_id, incoming) = nostr::build_signed(
        &poster,
        1,
        json!([["p", gate_hex]]),
        &case.first_input(),
        nostr::now_secs(),
    );
    relay.publish(incoming);

    if matches!(case, Case::History) {
        let first_turn = wait_for_turn(&store, place).await;
        assert_eq!(first_turn.end_reason, "done");
        let first_posts = wait_for_quiescent_posts(&relay, &store, place, 1).await;
        let first_post = &first_posts[0];
        assert_eq!(
            first_post["content"],
            json!("mock history seed acknowledged")
        );
        let first_post_id = first_post["id"]
            .as_str()
            .expect("first outbound note has an id")
            .to_string();

        // Relay が返した outbound note id だけを返信先にする。p-tag は付けないため、この二段目が即応するには
        // gate が e-tag を origin に写し、core が ack 済み outbound external ref へ対応づける必要がある。
        let (_question_id, question) = nostr::build_signed(
            &poster,
            1,
            json!([["e", first_post_id]]),
            "synthetic history question",
            nostr::now_secs(),
        );
        relay.publish(question);

        let turns = wait_for_turns(&store, place, 2).await;
        let posts = wait_for_quiescent_posts(&relay, &store, place, 2).await;
        assert_eq!(
            posts[1]["content"],
            json!("mock remembered synthetic history seed")
        );
        assert_eq!(turns[1].end_reason, "done");
        let records = store.context_records(turns[1].id).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].ctx_from_seq, Some(1));
        assert_eq!(records[0].ctx_to_seq, Some(3));

        drop(store);
        drop(core);
        drop(gate);
        drop(relay);
        return;
    }

    let turn = wait_for_turn(&store, place).await;

    match case.expected_reply() {
        Some(expected) => {
            let posts = wait_for_quiescent_posts(&relay, &store, place, 1).await;
            let post = &posts[0];
            nostr::verify_event(post).expect("gate produced a valid signed Nostr event");
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
            wait_for_quiescent_posts(&relay, &store, place, 0).await;
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
async fn fake_clock_exposes_immediate_and_fixed_window_batch_shape() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let scratch = E2eScratch::new();
    let mut relay = SyntheticRelay::start(&scratch.path().join("relay.log")).await;
    let socket = scratch.path().join("core.sock");
    let clock_socket = scratch.path().join("clock.sock");
    let db = scratch.path().join("core.db");
    let places = scratch.path().join("places.json");
    assert!(
        socket.as_os_str().len() < 100,
        "Unix socket path is too long"
    );
    assert!(
        clock_socket.as_os_str().len() < 100,
        "clock socket path is too long"
    );

    let poster = Key::generate();
    let (gate, gate_npub) =
        spawn_gate(&socket, &relay.url, &scratch.path().join("gate.stderr.log"));
    let address = format!("filter:kind=1&author={}", poster.npub);
    let batch_window = Duration::from_secs(60);
    let config = json!({
        "places": [{
            "address": address,
            "gate": NOSTR,
            "name": "synthetic-clock-agent",
            "persona": "You are a synthetic clock E2E agent.",
            "policy": {
                "immediate": ["mentions_me", "replies_to_me"],
                "immediate_from": "anyone",
                "batch_window_ms": batch_window.as_millis() as i64,
                "unconditional_interval_ms": null
            },
            "identities": [{"gate": NOSTR, "external": gate_npub}]
        }]
    });
    std::fs::write(&places, config.to_string()).expect("write places config");
    let core = spawn_core(
        &socket,
        &db,
        &places,
        "clock_batch",
        Some(&clock_socket),
        &scratch.path().join("core.stderr.log"),
    );

    let filter = relay.wait_for_hello_ready_subscription().await;
    assert_eq!(filter["kinds"], json!([1]));
    assert_eq!(filter["authors"], json!([poster.pubkey_hex.clone()]));
    let gate_hex = nostr::npub_to_hex(
        config["places"][0]["identities"][0]["external"]
            .as_str()
            .expect("gate identity"),
    )
    .expect("gate npub to hex");
    let mut clock = ClockControl::connect(&clock_socket).await;
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

    // 即応層は fake clock を一度も進めなくても publish する。
    let (_, immediate) = nostr::build_signed(
        &poster,
        1,
        json!([["p", gate_hex]]),
        "synthetic immediate clock probe",
        nostr::now_secs(),
    );
    relay.publish(immediate);
    wait_for_turns(&store, place, 1).await;
    let immediate_posts = wait_for_quiescent_posts(&relay, &store, place, 1).await;
    assert_eq!(
        immediate_posts[0]["content"],
        json!("mock immediate clock reply")
    );

    // 同じ unknown standing の通常投稿を二件積む。入力受理までは DB observer で同期するが、合否は relay
    // publish の外形（窓前 0 件、窓後 exact-one、その一回が二件を読んだ mock 応答）で判定する。
    for text in ["synthetic batched first", "synthetic batched second"] {
        let (_, event) = nostr::build_signed(&poster, 1, json!([]), text, nostr::now_secs());
        relay.publish(event);
    }
    wait_for_event_text(&store, place, "synthetic batched second").await;
    wait_for_quiescent_posts(&relay, &store, place, 1).await;

    clock.advance(batch_window - Duration::from_millis(1)).await;
    wait_for_quiescent_posts(&relay, &store, place, 1).await;

    clock.advance(Duration::from_millis(1)).await;
    wait_for_turns(&store, place, 2).await;
    let posts = wait_for_quiescent_posts(&relay, &store, place, 2).await;
    assert_eq!(posts[1]["content"], json!("mock batched pair"));

    drop(store);
    drop(core);
    drop(gate);
    drop(relay);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mention_context_identifies_trigger_and_posts_mock_reply() {
    run_case(Case::Reply).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_turn_answers_from_previously_read_conversation_history() {
    run_case(Case::History).await;
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
