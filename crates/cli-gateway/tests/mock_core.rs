use std::path::PathBuf;
use std::time::Duration;

use opencrab_cli_gateway::args::SessionArg;
use opencrab_cli_gateway::config::{InstancePlacement, Placement};
use opencrab_cli_gateway::runtime::{run, Frontend, RunOptions};
use opencrab_gate_client::wire::{read_frame, write_json};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

const INSTANCE_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const BINDING_ID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const MESSAGE_ID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const ADDRESS: &str = "extgate-cccccccc-cccc-4ccc-8ccc-cccccccccccc";

fn options(socket: PathBuf, session: SessionArg) -> RunOptions {
    let instance = InstancePlacement {
        instance_id: INSTANCE_ID.into(),
        revision: 1,
        agent_id: "agent-a".into(),
        author_id: "local-operator".into(),
    };
    RunOptions {
        placement: Placement {
            core_socket: socket.to_string_lossy().into_owned(),
            instances: vec![instance.clone()],
        },
        instance,
        session,
        frontend: Frontend::Jsonl,
        connect_timeout: Duration::from_secs(2),
    }
}

async fn read_value(stream: &mut UnixStream) -> Value {
    let frame = read_frame(stream).await.unwrap();
    serde_json::from_slice(&frame).unwrap()
}

async fn hello(stream: &mut UnixStream) {
    let frame = read_value(stream).await;
    assert_eq!(frame["m"], "hello");
    assert_eq!(frame["instance_id"], INSTANCE_ID);
    write_json(stream, &json!({"id": frame["id"], "m": "ok"}))
        .await
        .unwrap();
}

async fn bind(stream: &mut UnixStream, binding_id: &str, address: &str) {
    write_json(
        stream,
        &json!({
            "id": "bind:1",
            "m": "bind",
            "binding_id": binding_id,
            "address": address
        }),
    )
    .await
    .unwrap();
    let response = read_value(stream).await;
    assert_eq!(response, json!({"id": "bind:1", "m": "ok"}));
}

async fn assert_said_and_accept(stream: &mut UnixStream, address: &str) {
    let said = read_value(stream).await;
    assert_eq!(said["m"], "said");
    assert_eq!(said["origin"], format!("cli:{MESSAGE_ID}"));
    assert_eq!(said["author_id"], "local-operator");
    assert_eq!(said["caller"], json!({"role": "owner"}));
    assert_eq!(said["start_turn"], true);
    assert_eq!(said["live_inbound_scope"], "all");
    assert_eq!(said["text"], "hello");
    assert!(said["attachments"].as_array().unwrap().is_empty());
    assert!(!said.as_object().unwrap().contains_key("system_context"));
    assert!(!said.as_object().unwrap().contains_key("reply_target"));
    let _ = address;
    write_json(stream, &json!({"id": said["id"], "m": "ok", "seq": 7}))
        .await
        .unwrap();
}

async fn run_case(
    socket: PathBuf,
    session: SessionArg,
    input_text: String,
) -> (
    Vec<Value>,
    anyhow::Result<opencrab_cli_gateway::runtime::Exit>,
) {
    let (mut input_writer, input_reader) = tokio::io::duplex(4096);
    let (output_writer, mut output_reader) = tokio::io::duplex(16 * 1024);
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        input_writer.write_all(input_text.as_bytes()).await.unwrap();
        input_writer.shutdown().await.unwrap();
    });
    let (_control_tx, control_rx) = mpsc::channel(1);
    let run_task = tokio::spawn(run(
        options(socket, session),
        input_reader,
        output_writer,
        control_rx,
    ));
    let mut output = Vec::new();
    output_reader.read_to_end(&mut output).await.unwrap();
    let result = run_task.await.unwrap();
    let records = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    (records, result)
}

#[tokio::test]
async fn attached_session_sends_exact_owner_context_and_ordered_acceptance() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        hello(&mut stream).await;
        bind(&mut stream, BINDING_ID, ADDRESS).await;
        assert_said_and_accept(&mut stream, ADDRESS).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });
    let input = format!(
        "{{\"type\":\"message\",\"id\":\"{MESSAGE_ID}\",\"text\":\"hello\"}}\n{{\"type\":\"shutdown\"}}\n"
    );
    let (records, result) = run_case(socket, SessionArg::Existing(ADDRESS.into()), input).await;
    result.unwrap();
    server.await.unwrap();
    assert_eq!(records[0]["type"], "ready");
    assert_eq!(records[0]["session_id"], ADDRESS);
    assert_eq!(records[1]["type"], "accepted");
    assert_eq!(records[1]["seq"], 7);
    assert_eq!(records[2], json!({"type": "closed", "reason": "requested"}));
}

#[tokio::test]
async fn disconnect_reconnects_and_never_replays_uncertain_input() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (rebound_tx, rebound_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        hello(&mut first).await;
        bind(&mut first, BINDING_ID, ADDRESS).await;
        let said = read_value(&mut first).await;
        assert_eq!(said["origin"], format!("cli:{MESSAGE_ID}"));
        drop(first);

        let (mut second, _) = listener.accept().await.unwrap();
        hello(&mut second).await;
        bind(&mut second, BINDING_ID, ADDRESS).await;
        rebound_tx.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(150), read_value(&mut second))
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    let (mut input_writer, input_reader) = tokio::io::duplex(4096);
    let (output_writer, mut output_reader) = tokio::io::duplex(16 * 1024);
    use tokio::io::AsyncWriteExt;
    input_writer
        .write_all(
            format!("{{\"type\":\"message\",\"id\":\"{MESSAGE_ID}\",\"text\":\"hello\"}}\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let (control_tx, control_rx) = mpsc::channel(1);
    let run_task = tokio::spawn(run(
        options(socket, SessionArg::Existing(ADDRESS.into())),
        input_reader,
        output_writer,
        control_rx,
    ));
    rebound_rx.await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    input_writer
        .write_all(b"{\"type\":\"shutdown\"}\n")
        .await
        .unwrap();
    input_writer.shutdown().await.unwrap();
    let mut output = Vec::new();
    output_reader.read_to_end(&mut output).await.unwrap();
    run_task.await.unwrap().unwrap();
    drop(control_tx);
    server.await.unwrap();
    let records: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert!(records
        .iter()
        .any(|record| { record["type"] == "error" && record["code"] == "disconnect" }));
    assert!(records
        .iter()
        .any(|record| { record["type"] == "connection" && record["state"] == "disconnected" }));
    assert!(records
        .iter()
        .any(|record| { record["type"] == "connection" && record["state"] == "connected" }));
    assert!(!records.iter().any(|record| record["type"] == "accepted"));
}

#[tokio::test]
async fn new_session_uses_generated_equal_address_and_binding_then_announces_it() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        hello(&mut stream).await;
        let create = read_value(&mut stream).await;
        assert_eq!(create["m"], "create_binding");
        assert_eq!(create["session_theme"], "Terminal chat");
        let binding_id = create["binding_id"].as_str().unwrap().to_string();
        let address = create["address"].as_str().unwrap().to_string();
        assert_eq!(address, format!("extgate-{binding_id}"));
        write_json(&mut stream, &json!({"id": create["id"], "m": "ok"}))
            .await
            .unwrap();
        bind(&mut stream, &binding_id, &address).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });
    let (records, result) = run_case(
        socket,
        SessionArg::New("Terminal chat".into()),
        "{\"type\":\"shutdown\"}\n".into(),
    )
    .await;
    result.unwrap();
    server.await.unwrap();
    assert_eq!(records[0]["type"], "ready");
    assert!(records[0]["session_id"]
        .as_str()
        .unwrap()
        .starts_with("extgate-"));
    assert_eq!(records[1]["type"], "closed");
}
