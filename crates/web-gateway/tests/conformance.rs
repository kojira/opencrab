//! DESIGN-SAMPLES-NODE §1: process harness。SUT は argv[1]=placement.json。
//! InstanceClient / router() / parse_frame_bytes は import しない。

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener as StdTcp};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const STEP_TIMEOUT: Duration = Duration::from_secs(5);

struct Sut(Child);
impl Drop for Sut {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct MockCore {
    sock: PathBuf,
    listener: Option<UnixListener>,
    writer: Option<OwnedWriteHalf>,
    incoming: mpsc::UnboundedReceiver<Value>,
    tx: mpsc::UnboundedSender<Value>,
    reader: Option<JoinHandle<()>>,
}

impl MockCore {
    fn bind(sock: PathBuf) -> Self {
        let listener = UnixListener::bind(&sock).expect("bind mock core");
        let (tx, incoming) = mpsc::unbounded_channel();
        Self {
            sock,
            listener: Some(listener),
            writer: None,
            incoming,
            tx,
            reader: None,
        }
    }

    async fn accept(&mut self) {
        let listener = self.listener.as_ref().expect("listener");
        let (stream, _) = tokio::time::timeout(STEP_TIMEOUT, listener.accept())
            .await
            .expect("accept timeout")
            .expect("accept");
        self.attach(stream);
    }

    fn attach(&mut self, stream: UnixStream) {
        let (mut reader, writer) = stream.into_split();
        self.writer = Some(writer);
        let tx = self.tx.clone();
        self.reader = Some(tokio::spawn(async move {
            while let Ok(v) = read_json_line(&mut reader).await {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }));
    }

    async fn send(&mut self, v: &Value) {
        let w = self.writer.as_mut().expect("writer");
        let mut bytes = serde_json::to_vec(v).expect("json");
        bytes.push(b'\n');
        w.write_all(&bytes).await.expect("uds write");
        w.flush().await.expect("uds flush");
    }

    async fn send_raw(&mut self, bytes: &[u8]) {
        let w = self.writer.as_mut().expect("writer");
        w.write_all(bytes).await.expect("uds raw");
        w.flush().await.expect("uds flush");
    }

    async fn recv(&mut self) -> Value {
        tokio::time::timeout(STEP_TIMEOUT, self.incoming.recv())
            .await
            .expect("uds recv timeout")
            .expect("uds closed")
    }

    async fn close_current(&mut self) {
        if let Some(mut w) = self.writer.take() {
            let _ = w.shutdown().await;
        }
        if let Some(task) = self.reader.take() {
            task.abort();
        }
    }

    fn unlisten(&mut self) {
        self.listener.take();
        let _ = std::fs::remove_file(&self.sock);
    }
}

async fn read_json_line(reader: &mut OwnedReadHalf) -> Result<Value, ()> {
    let mut buf = Vec::new();
    loop {
        let mut b = [0u8; 1];
        reader.read_exact(&mut b).await.map_err(|_| ())?;
        if b[0] == b'\n' {
            break;
        }
        buf.push(b[0]);
        if buf.len() > 2_097_152 {
            return Err(());
        }
    }
    serde_json::from_slice(&buf).map_err(|_| ())
}

struct SseConn {
    resp: reqwest::Response,
    buf: String,
}

struct Session {
    http: SocketAddr,
    client: reqwest::Client,
    mock: MockCore,
    _sut: Sut,
    captures: BTreeMap<String, Value>,
    pending_http: BTreeMap<String, JoinHandle<(u16, Value)>>,
    sse: BTreeMap<String, SseConn>,
    _dir: tempfile::TempDir,
}

fn sut_path() -> PathBuf {
    match std::env::var_os("OPENCRAB_CONFORMANCE_SUT") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_BIN_EXE_web-gateway")),
    }
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../conformance/fixtures")
}

fn load_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read fixture")).expect("json")
}

fn free_port() -> u16 {
    StdTcp::bind("127.0.0.1:0")
        .expect("port")
        .local_addr()
        .expect("addr")
        .port()
}

async fn wait_listen(addr: SocketAddr) {
    tokio::time::timeout(STEP_TIMEOUT, async {
        loop {
            if TcpStream::connect(addr).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("http_bind listen not ready");
}

fn subset(expect: &Value, actual: &Value) -> bool {
    match (expect, actual) {
        (Value::Object(e), Value::Object(a)) => e
            .iter()
            .all(|(k, v)| a.get(k).is_some_and(|got| subset(v, got))),
        (Value::Array(e), Value::Array(a)) if e.len() == a.len() => {
            e.iter().zip(a.iter()).all(|(ev, av)| subset(ev, av))
        }
        (e, a) => e == a,
    }
}

fn interp(v: &Value, caps: &BTreeMap<String, Value>) -> Value {
    match v {
        Value::String(s) if s.starts_with('$') => lookup(s, caps),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, val)| (k.clone(), interp(val, caps)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|val| interp(val, caps)).collect()),
        other => other.clone(),
    }
}

fn lookup(expr: &str, caps: &BTreeMap<String, Value>) -> Value {
    let mut parts = expr.trim_start_matches('$').split('.');
    let root = parts.next().expect("capture");
    let mut cur = caps
        .get(root)
        .unwrap_or_else(|| panic!("missing capture {root}"));
    for key in parts {
        cur = cur
            .get(key)
            .unwrap_or_else(|| panic!("missing {expr}"));
    }
    cur.clone()
}

fn take_sse_event(buf: &mut String) -> Option<(String, Value)> {
    let idx = buf.find("\n\n")?;
    let block = buf[..idx].to_string();
    *buf = buf[idx + 2..].to_string();
    let mut event = String::from("message");
    let mut data = String::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix(':') {
            let _ = rest;
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }
    if event.is_empty() && data.is_empty() {
        return take_sse_event(buf);
    }
    let payload = if data.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&data).unwrap_or(Value::String(data))
    };
    Some((event, payload))
}

impl Session {
    async fn start(ids: &Value) -> Self {
        let dir = tempfile::tempdir().expect("tmp");
        let sock = dir.path().join("core.sock");
        let sock_str = sock.to_str().expect("utf8 sock").to_string();
        let mut mock = MockCore::bind(sock);
        let port = free_port();
        let http: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let placement = json!({
            "http_bind": http.to_string(),
            "core_socket": sock_str,
            "instances": [{
                "instance_id": ids["instance_id"],
                "revision": ids["revision"],
                "author_id": ids["author_id"],
            }]
        });
        let place_path = dir.path().join("placement.json");
        std::fs::write(&place_path, serde_json::to_vec(&placement).unwrap()).unwrap();
        let child = Command::new(sut_path())
            .arg(&place_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn web-gateway");
        let sut = Sut(child);
        wait_listen(http).await;
        mock.accept().await;
        let mut captures = BTreeMap::new();
        captures.insert("ids".into(), ids.clone());
        Self {
            http,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            mock,
            _sut: sut,
            captures,
            pending_http: BTreeMap::new(),
            sse: BTreeMap::new(),
            _dir: dir,
        }
    }

    async fn prelude(&mut self, kind: &str, ids: &Value) {
        if kind == "none" {
            return;
        }
        let hello = self.mock.recv().await;
        let expect = json!({
            "m": "hello",
            "protocol": 2,
            "instance_id": ids["instance_id"],
            "revision": ids["revision"],
            "config_digest": ids["config_digest"],
        });
        assert!(subset(&expect, &hello), "hello {hello} !~ {expect}");
        self.captures.insert("hello".into(), hello.clone());
        self.mock
            .send(&json!({"id": hello["id"], "m": "ok"}))
            .await;
        if kind == "hello" {
            return;
        }
        assert_eq!(kind, "hello_bind");
        let bind_id = format!("bind:{}", ids["binding_id"].as_str().unwrap());
        self.mock
            .send(&json!({
                "id": bind_id,
                "m": "bind",
                "binding_id": ids["binding_id"],
                "address": ids["address"],
            }))
            .await;
        let ack = self.mock.recv().await;
        assert!(
            subset(&json!({"m": "ok", "id": bind_id}), &ack),
            "bind ack {ack}"
        );
    }

    async fn http_exchange(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let url = format!("http://{}{path}", self.http);
        let mut req = match method {
            "POST" => self.client.post(&url),
            "GET" => self.client.get(&url),
            other => panic!("method {other}"),
        };
        req = req.timeout(STEP_TIMEOUT);
        if let Some(b) = body {
            req = req
                .header("content-type", "application/json")
                .body(b.to_string());
        }
        let resp = req.send().await.expect("http");
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.expect("body");
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    async fn sse_open(&mut self, id: &str, path: &str) {
        let url = format!("http://{}{path}", self.http);
        let resp = self
            .client
            .get(&url)
            .header("accept", "text/event-stream")
            .send()
            .await
            .expect("sse");
        assert!(
            resp.status().is_success(),
            "sse status {}",
            resp.status()
        );
        self.sse.insert(
            id.to_string(),
            SseConn {
                resp,
                buf: String::new(),
            },
        );
    }

    async fn sse_recv(&mut self, id: &str) -> (String, Value) {
        let conn = self.sse.get_mut(id).unwrap_or_else(|| panic!("sse {id}"));
        tokio::time::timeout(STEP_TIMEOUT, async {
            loop {
                if let Some(ev) = take_sse_event(&mut conn.buf) {
                    return ev;
                }
                let chunk = conn.resp.chunk().await.expect("sse chunk");
                let Some(chunk) = chunk else {
                    panic!("sse eof before event");
                };
                conn.buf.push_str(&String::from_utf8_lossy(&chunk));
            }
        })
        .await
        .expect("sse recv timeout")
    }

    async fn step(&mut self, step: &Value) {
        let op = step["op"].as_str().expect("op");
        match op {
            "http_post_async" => {
                let id = step["id"].as_str().unwrap().to_string();
                let path = step["path"].as_str().unwrap().to_string();
                let body = interp(&step["body"], &self.captures);
                let client = self.client.clone();
                let http = self.http;
                self.pending_http.insert(
                    id,
                    tokio::spawn(async move {
                        let url = format!("http://{http}{path}");
                        let resp = client
                            .post(&url)
                            .timeout(STEP_TIMEOUT)
                            .header("content-type", "application/json")
                            .body(body.to_string())
                            .send()
                            .await
                            .expect("post");
                        let status = resp.status().as_u16();
                        let bytes = resp.bytes().await.expect("body");
                        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                        (status, value)
                    }),
                );
            }
            "http_await" => {
                let id = step["id"].as_str().unwrap();
                let handle = self.pending_http.remove(id).expect("pending");
                let (status, body) = handle.await.expect("join");
                let want = step["status"].as_u64().unwrap() as u16;
                assert_eq!(status, want, "http {id} status {status} body {body}");
                if let Some(exp) = step.get("body") {
                    let exp = interp(exp, &self.captures);
                    assert!(subset(&exp, &body), "http {id} {body} !~ {exp}");
                }
            }
            "http_post" => {
                let path = step["path"].as_str().unwrap();
                let body = interp(&step["body"], &self.captures);
                let (status, got) = self.http_exchange("POST", path, Some(body)).await;
                let want = step["status"].as_u64().unwrap() as u16;
                assert_eq!(status, want, "POST {path} {status} {got}");
                if let Some(exp) = step.get("body_expect") {
                    let exp = interp(exp, &self.captures);
                    assert!(subset(&exp, &got), "POST {path} {got} !~ {exp}");
                }
            }
            "http_get" => {
                let path = step["path"].as_str().unwrap();
                let (status, _) = self.http_exchange("GET", path, None).await;
                let want = step["status"].as_u64().unwrap() as u16;
                assert_eq!(status, want, "GET {path}");
            }
            "uds_recv" => {
                let got = self.mock.recv().await;
                let expect = interp(&step["expect"], &self.captures);
                assert!(subset(&expect, &got), "uds {got} !~ {expect}");
                if let Some(id) = step.get("id").and_then(Value::as_str) {
                    self.captures.insert(id.to_string(), got);
                }
            }
            "uds_send" => {
                let frame = interp(&step["frame"], &self.captures);
                self.mock.send(&frame).await;
            }
            "uds_send_raw" => {
                let utf8 = step["utf8"].as_str().unwrap();
                self.mock.send_raw(utf8.as_bytes()).await;
            }
            "uds_send_oversized" => {
                let byte = u8::try_from(step["byte"].as_u64().unwrap()).unwrap();
                let count = usize::try_from(step["count"].as_u64().unwrap()).unwrap();
                let mut raw = vec![byte; count];
                if step["nl"].as_bool().unwrap_or(false) {
                    raw.push(b'\n');
                }
                self.mock.send_raw(&raw).await;
            }
            "uds_close" => self.mock.close_current().await,
            "uds_unlisten" => self.mock.unlisten(),
            "uds_accept" => self.mock.accept().await,
            "sse_open" => {
                let id = step["id"].as_str().unwrap();
                let path = step["path"].as_str().unwrap();
                self.sse_open(id, path).await;
            }
            "sse_recv" => {
                let id = step["id"].as_str().unwrap();
                let (event, data) = self.sse_recv(id).await;
                let want_ev = step["event"].as_str().unwrap();
                assert_eq!(event, want_ev, "sse event {event} data {data}");
                if let Some(exp) = step.get("data") {
                    let exp = interp(exp, &self.captures);
                    assert!(subset(&exp, &data), "sse data {data} !~ {exp}");
                }
            }
            other => panic!("unknown op {other}"),
        }
    }
}

async fn run_named(name: &str) {
    let dir = fixtures_dir();
    let ids = load_json(&dir.join("ids.json"));
    let fixture = load_json(&dir.join(format!("{name}.json")));
    assert_eq!(fixture["name"].as_str().unwrap(), name);
    let mut session = Session::start(&ids).await;
    session
        .prelude(fixture["prelude"].as_str().unwrap(), &ids)
        .await;
    for step in fixture["steps"].as_array().unwrap() {
        session.step(step).await;
    }
}

#[tokio::test]
async fn hello_bind_said_dedup() {
    run_named("hello-bind-said-dedup").await;
}

#[tokio::test]
async fn say_three_results() {
    run_named("say-three-results").await;
}

#[tokio::test]
async fn activity() {
    run_named("activity").await;
}

#[tokio::test]
async fn frame_too_large() {
    run_named("frame-too-large").await;
}

#[tokio::test]
async fn frame_duplicate() {
    run_named("frame-duplicate").await;
}

#[tokio::test]
async fn http_post_and_routes() {
    run_named("http-post-and-routes").await;
}

#[tokio::test]
async fn disconnect_unacked() {
    run_named("disconnect-unacked").await;
}

#[tokio::test]
async fn bind_conflict() {
    run_named("bind-conflict").await;
}

#[tokio::test]
async fn reconnect() {
    run_named("reconnect").await;
}
