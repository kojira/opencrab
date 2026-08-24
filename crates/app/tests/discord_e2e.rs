//! TEST-DESIGN B8: crate/bin の存在と、実 Discord 無しの mock-level 往復。
//! wire 対実 Discord は人間 QC。synthetic Discord server は置かない。

use opencrab_discord_gate::{protocol2_hello, KIND_ID};
use opencrab_port::GateInstanceId;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::time::timeout;

const INSTANCE: &str = "018f0000-0000-7000-8000-000000000061";

fn bin_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_discord-gate-e2e"))
        .parent()
        .expect("e2e binary parent")
        .to_path_buf()
}

#[tokio::test]
async fn crate_exists_and_boot_error_reports_failed_after_hello() {
    assert_eq!(KIND_ID, "discord");
    let instance = GateInstanceId::parse(INSTANCE.to_string()).unwrap();
    let expected_hello = protocol2_hello(&instance, 1);

    let dir = bin_dir().join(format!("de2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("core.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).expect("bind fake core");

    let mut child = Command::new(env!("CARGO_BIN_EXE_discord-gate-e2e"))
        .env("OPENCRAB_GATE_INSTANCE_ID", INSTANCE)
        .env("OPENCRAB_GATE_REVISION", "1")
        .env("OPENCRAB_GATE_SOCKET", socket.as_os_str())
        .env("OPENCRAB_GATE_BOOT_ERROR_CODE", "missing_secret")
        .env_remove("OPENCRAB_GATE_TOKEN")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn discord-gate-e2e");

    let accept = timeout(Duration::from_secs(5), listener.accept());
    let (stream, _) = accept.await.expect("accept timeout").expect("accept");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let hello_line = timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("hello timeout")
        .expect("hello read")
        .expect("hello line");
    let hello: Value = serde_json::from_str(&hello_line).expect("hello json");
    assert_eq!(hello, expected_hello);

    writer
        .write_all(
            format!(
                "{}\n",
                json!({"id":"hello-1","ok":{"protocol":2,"connection_epoch":1}})
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    writer.flush().await.unwrap();

    let failed_line = timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("failed timeout")
        .expect("failed read")
        .expect("failed line");
    let failed: Value = serde_json::from_str(&failed_line).expect("failed json");
    assert_eq!(failed["id"], "failed-1");
    assert_eq!(failed["m"], "failed");
    assert_eq!(failed["connection_epoch"], 1);
    assert_eq!(failed["code"], "missing_secret");
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
