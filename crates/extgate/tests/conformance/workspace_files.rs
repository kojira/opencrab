// ===== D-1079a: declared `format:"workspace-file"` fields → core-owned outbox descriptors =====

/// Utterance op with a marked array field (`files`), a marked-looking but undeclared field is
/// not rewritten, and an unmarked string passes through.
fn ops_with_workspace_files() -> Value {
    json!([
        {
            "name": "post_with_files", "description": "d",
            "input_schema": {"type": "object", "required": ["text"], "properties": {
                "text": {"type": "string"},
                "label": {"type": "string"},
                "files": {"type": "array", "items": {"type": "string", "format": "workspace-file"}}
            }},
            "output_schema": null, "callback_schema": null,
            "authorization": {"allowed_callers": ["co_agent", "guest", "owner", "trusted"]},
            "dispatch": "utterance", "sub_engine": "not_exposed",
            "sharing": "conversation_bound", "effect": "utterance"
        },
        {
            "name": "store_file", "description": "d",
            "input_schema": {"type": "object", "required": ["file"], "properties": {
                "file": {"type": "string", "format": "workspace-file"}
            }},
            "output_schema": null, "callback_schema": null,
            "authorization": {"allowed_callers": ["co_agent", "guest", "owner", "trusted"]},
            "dispatch": "background", "sub_engine": "not_exposed",
            "sharing": "conversation_bound", "effect": "state_change"
        }
    ])
}

struct FileFixture {
    h: Harness,
    s: UnixStream,
    binding_id: String,
    session_id: String,
    proj: ExtgateOpsGatewayActions,
    workspace: std::path::PathBuf,
    outbox: std::path::PathBuf,
    _files: tempfile::TempDir,
}

async fn file_fixture(configure_outbox: bool) -> FileFixture {
    let h = Harness::start().await;
    let files = tempfile::tempdir().unwrap();
    let root = files.path().canonicalize().unwrap();
    let workspace = root.join("agents/agent-1/workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let outbox = root.join("outbox");
    std::fs::create_dir_all(&outbox).unwrap();
    h.state.set_workspace_base(
        root.join("agents/{agent_id}/workspace")
            .to_string_lossy()
            .into_owned(),
    );
    if configure_outbox {
        h.state.set_attachment_outbox_root(outbox.clone());
    }
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-files").await;
    let mut s = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s, &instance_id, 1, &ops_with_workspace_files()).await["m"],
        "ok"
    );
    assert_eq!(ack_bind(&mut s).await, binding_id);
    wait_acked(&h, &instance_id, &binding_id).await;
    let session_id = session_id_for_binding(&binding_id);
    let proj = ExtgateOpsGatewayActions::for_binding(
        Arc::clone(&h.state),
        &instance_id,
        &binding_id,
        &session_id,
        "agent-1",
    )
    .unwrap();
    FileFixture {
        h,
        s,
        binding_id,
        session_id,
        proj,
        workspace,
        outbox,
        _files: files,
    }
}

fn outbox_files(outbox: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(outbox).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            for inner in std::fs::read_dir(&path).unwrap() {
                out.push(inner.unwrap().path());
            }
        } else {
            out.push(path);
        }
    }
    out
}

fn count_rows(h: &Harness, sql: &str, binding_or_session: &str) -> i64 {
    h.state
        .db
        .lock()
        .unwrap()
        .query_row(sql, [binding_or_session], |r| r.get(0))
        .unwrap()
}

async fn wait_outbox_empty(outbox: &std::path::Path) {
    for _ in 0..200 {
        if outbox_files(outbox).is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("outbox copies not removed: {:?}", outbox_files(outbox));
}

/// Marked array field becomes descriptors (copy under the outbox with matching size/sha),
/// unmarked and undeclared fields pass through, speech_meta records names, deliveries stays
/// `{text}`, and an `ok` response removes the outbox copies.
#[tokio::test]
async fn workspace_file_utterance_rewrites_marked_field_and_cleans_up_on_ok() {
    let mut f = file_fixture(true).await;
    std::fs::create_dir_all(f.workspace.join("generated_images")).unwrap();
    std::fs::write(f.workspace.join("generated_images/x.png"), b"\x89PNG\r\n\x1a\nimg").unwrap();
    std::fs::write(f.workspace.join("notes.txt"), b"hello").unwrap();

    let ctx = GatewayCallContext::for_agent("agent-1");
    let result = f
        .proj
        .execute(
            "post_with_files",
            &json!({
                "text": "here",
                "label": "notes.txt",
                "other_files": ["notes.txt"],
                "files": ["generated_images/x.png", "./notes.txt"]
            }),
            &ctx,
        )
        .await;
    assert!(result.success, "{:?}", result.error);

    let invoke = read_until(&mut f.s, |v| v["m"] == "invoke").await;
    let payload = &invoke["payload"];
    assert_eq!(payload["text"], "here");
    assert_eq!(payload["label"], "notes.txt", "unmarked field passes through");
    assert_eq!(payload["other_files"], json!(["notes.txt"]), "undeclared field untouched");
    let files = payload["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    let expect = [
        ("x.png", Some("image/png"), b"\x89PNG\r\n\x1a\nimg".as_slice()),
        ("notes.txt", Some("text/plain"), b"hello".as_slice()),
    ];
    for (descriptor, (name, media_type, bytes)) in files.iter().zip(expect) {
        let obj = descriptor.as_object().unwrap();
        let mut keys: Vec<_> = obj.keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["id", "local_path", "media_type", "name", "sha256", "size"]
        );
        assert_eq!(descriptor["name"], name);
        assert_eq!(descriptor["media_type"], json!(media_type));
        assert_eq!(descriptor["size"], bytes.len() as u64);
        let sha = format!("{:x}", Sha256::digest(bytes));
        assert_eq!(descriptor["sha256"], sha);
        let local = descriptor["local_path"].as_str().unwrap();
        assert!(std::path::Path::new(local).is_relative());
        let copy = f.outbox.join(local);
        assert_eq!(std::fs::read(&copy).unwrap(), bytes);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let delivery: String = f
        .h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT payload_json FROM deliveries WHERE binding_id = ?1",
            [&f.binding_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&delivery).unwrap(), json!({"text": "here"}));
    let logs = {
        let conn = f.h.state.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &f.session_id).unwrap()
    };
    let speech = logs.iter().find(|l| l.log_type == "speech").unwrap();
    let meta: Value = serde_json::from_str(speech.metadata_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        meta["attachments"],
        json!([{"name": "x.png"}, {"name": "notes.txt"}])
    );

    let call_id = invoke["id"].as_str().unwrap().to_string();
    write_frame(&mut f.s, &json!({"id": call_id, "m": "ok"})).await;
    wait_outbox_empty(&f.outbox).await;
}

/// `err(operation_rejected)` is terminal: the outbox copies are removed.
#[tokio::test]
async fn workspace_file_copies_removed_on_err() {
    let mut f = file_fixture(true).await;
    std::fs::write(f.workspace.join("a.txt"), b"a").unwrap();
    let ctx = GatewayCallContext::for_agent("agent-1");
    let result = f
        .proj
        .execute("post_with_files", &json!({"text": "t", "files": ["a.txt"]}), &ctx)
        .await;
    assert!(result.success, "{:?}", result.error);
    let invoke = read_until(&mut f.s, |v| v["m"] == "invoke").await;
    assert_eq!(outbox_files(&f.outbox).len(), 1);
    let call_id = invoke["id"].as_str().unwrap().to_string();
    write_frame(
        &mut f.s,
        &json!({"id": call_id, "m": "err", "code": "operation_rejected", "detail": null}),
    )
    .await;
    wait_outbox_empty(&f.outbox).await;
}

/// Connection close (indeterminate) is terminal for both utterance and awaited invokes.
#[tokio::test]
async fn workspace_file_copies_removed_on_close() {
    let mut f = file_fixture(true).await;
    std::fs::write(f.workspace.join("a.txt"), b"a").unwrap();
    std::fs::write(f.workspace.join("b.bin"), b"bb").unwrap();
    let ctx = GatewayCallContext::for_agent("agent-1");
    let result = f
        .proj
        .execute("post_with_files", &json!({"text": "t", "files": ["a.txt"]}), &ctx)
        .await;
    assert!(result.success, "{:?}", result.error);
    let _ = read_until(&mut f.s, |v| v["m"] == "invoke").await;
    let proj = f.proj;
    let awaited = tokio::spawn(async move {
        proj.execute("store_file", &json!({"file": "b.bin"}), &ctx).await
    });
    let invoke = read_until(&mut f.s, |v| v["m"] == "invoke").await;
    assert_eq!(invoke["payload"]["file"]["name"], "b.bin", "single marked field rewritten");
    assert_eq!(outbox_files(&f.outbox).len(), 2);
    drop(f.s);
    let result = tokio::time::timeout(Duration::from_secs(5), awaited)
        .await
        .unwrap()
        .unwrap();
    assert!(!result.success);
    wait_outbox_empty(&f.outbox).await;
}

/// Awaited (background) invoke: an `ok` response removes the copy.
#[tokio::test]
async fn workspace_file_copies_removed_on_awaited_ok() {
    let mut f = file_fixture(true).await;
    std::fs::write(f.workspace.join("b.bin"), b"bb").unwrap();
    let ctx = GatewayCallContext::for_agent("agent-1");
    let proj = f.proj;
    let awaited = tokio::spawn(async move {
        proj.execute("store_file", &json!({"file": "b.bin"}), &ctx).await
    });
    let invoke = read_until(&mut f.s, |v| v["m"] == "invoke").await;
    assert_eq!(outbox_files(&f.outbox).len(), 1);
    let call_id = invoke["id"].as_str().unwrap().to_string();
    write_frame(&mut f.s, &json!({"id": call_id, "m": "ok", "result": null})).await;
    let result = tokio::time::timeout(Duration::from_secs(5), awaited)
        .await
        .unwrap()
        .unwrap();
    assert!(result.success, "{:?}", result.error);
    wait_outbox_empty(&f.outbox).await;
}

/// Every rejection returns a tool error before any deliveries row, speech row or invoke frame,
/// and leaves no outbox copy (including copies of earlier valid entries in the same list).
#[tokio::test]
async fn workspace_file_rejections_have_zero_side_effects() {
    let mut f = file_fixture(true).await;
    let outside = f.workspace.parent().unwrap().join("outside.txt");
    std::fs::write(&outside, b"secret").unwrap();
    std::fs::write(f.workspace.join("ok.txt"), b"ok").unwrap();
    std::fs::create_dir_all(f.workspace.join("dir")).unwrap();
    std::os::unix::fs::symlink(&outside, f.workspace.join("leaf-link.txt")).unwrap();
    std::os::unix::fs::symlink(f.workspace.join("ok.txt"), f.workspace.join("inner-link.txt"))
        .unwrap();
    let real_dir = f.workspace.join("real");
    std::fs::create_dir_all(&real_dir).unwrap();
    std::fs::write(real_dir.join("f.txt"), b"f").unwrap();
    std::os::unix::fs::symlink(&real_dir, f.workspace.join("dir-link")).unwrap();
    let fifo = std::ffi::CString::new(f.workspace.join("pipe").into_os_string().into_encoded_bytes())
        .unwrap();
    // SAFETY: NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    let cases: Vec<(&str, Value)> = vec![
        ("traversal", json!(["ok.txt", "../outside.txt"])),
        ("absolute", json!([outside.to_string_lossy()])),
        ("symlink leaf (outside)", json!(["ok.txt", "leaf-link.txt"])),
        ("symlink leaf (inside)", json!(["inner-link.txt"])),
        ("symlinked dir", json!(["dir-link/f.txt"])),
        ("directory", json!(["dir"])),
        ("fifo", json!(["ok.txt", "pipe"])),
        ("missing", json!(["ok.txt", "nope.txt"])),
        ("non-string item", json!([1])),
        ("not an array", json!("ok.txt")),
    ];
    let ctx = GatewayCallContext::for_agent("agent-1");
    for (label, files) in cases {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            f.proj
                .execute("post_with_files", &json!({"text": "t", "files": files}), &ctx),
        )
        .await
        .unwrap_or_else(|_| panic!("{label}: must be rejected promptly"));
        assert!(!result.success, "{label}: must be rejected");
        let error = result.error.unwrap();
        assert!(error.starts_with("workspace_file_rejected: files"), "{label}: {error}");
        assert!(!error.contains(&*f.workspace.to_string_lossy()), "{label}: leaks host path");
        assert!(outbox_files(&f.outbox).is_empty(), "{label}: outbox copy left");
    }
    assert_eq!(
        count_rows(&f.h, "SELECT COUNT(*) FROM deliveries WHERE binding_id = ?1", &f.binding_id),
        0
    );
    assert_eq!(
        count_rows(
            &f.h,
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
            &f.session_id
        ),
        0
    );
    assert!(read_frame_opt(&mut f.s).await.is_none(), "no invoke frame written");
}

/// Using a marked field without a configured outbox fails closed (no fallback, no side effect);
/// payloads that do not use the marked field still work.
#[tokio::test]
async fn workspace_file_without_outbox_is_rejected() {
    let mut f = file_fixture(false).await;
    std::fs::write(f.workspace.join("ok.txt"), b"ok").unwrap();
    let ctx = GatewayCallContext::for_agent("agent-1");
    let result = f
        .proj
        .execute("post_with_files", &json!({"text": "t", "files": ["ok.txt"]}), &ctx)
        .await;
    assert!(!result.success);
    assert!(result.error.unwrap().contains("outbox is not configured"));
    assert_eq!(
        count_rows(&f.h, "SELECT COUNT(*) FROM deliveries WHERE binding_id = ?1", &f.binding_id),
        0
    );
    assert_eq!(
        count_rows(
            &f.h,
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
            &f.session_id
        ),
        0
    );
    assert!(read_frame_opt(&mut f.s).await.is_none(), "no invoke frame written");

    let result = f
        .proj
        .execute("post_with_files", &json!({"text": "plain"}), &ctx)
        .await;
    assert!(result.success, "{:?}", result.error);
    let invoke = read_until(&mut f.s, |v| v["m"] == "invoke").await;
    assert_eq!(invoke["payload"], json!({"text": "plain"}));
}
