//! D-1098: core starts with no `[gate_admin]` at all. Gateways provision over the ordinary gate
//! socket, so the separate admin socket and its credential manifest are not required.

use std::io::Read as _;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn health(port: u16) -> Option<String> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    std::io::Write::write_all(
        &mut stream,
        b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

#[test]
fn d1098_core_starts_and_serves_health_without_gate_admin_config() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let socket = dir.path().join("gate.sock");
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    std::fs::write(
        dir.path().join("config/default.toml"),
        format!(
            "[gateway.rest]\nenabled = true\nport = {port}\n\n\
             [database]\npath = \"{}\"\n\n\
             [gate]\nlisten_socket = \"{}\"\n",
            dir.path().join("data/opencrab.db").display(),
            socket.display(),
        ),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    // The startup budget check needs the default model's context window (unrelated to gate_admin).
    {
        let db =
            opencrab_db::Db::open(dir.path().join("data/opencrab.db").to_str().unwrap()).unwrap();
        let conn = db.lock().unwrap();
        opencrab_db::queries::upsert_model_pricing(
            &conn,
            &opencrab_db::queries::ModelPricingRow {
                provider: "openai".into(),
                model: "gpt-4o".into(),
                input_price_per_1m: 0.0,
                output_price_per_1m: 0.0,
                context_window: Some(128_000),
                max_output_tokens: Some(16_000),
                cached_input_price_per_1m: None,
                cache_write_price_per_1m: None,
            },
        )
        .unwrap();
    }
    let child = Command::new(env!("CARGO_BIN_EXE_opencrab-server"))
        .current_dir(dir.path())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut server = Server(child);

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            let mut stderr = String::new();
            if let Some(mut pipe) = server.0.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!("core exited ({status}) without [gate_admin]: {stderr}");
        }
        if let Some(response) = health(port) {
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "core did not serve /health in time"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(socket.exists(), "the ordinary gate socket is listening");
}
