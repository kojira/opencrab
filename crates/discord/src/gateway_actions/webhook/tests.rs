use super::*;
use serde_json::json;

fn meta() -> LifecycleMeta {
    LifecycleMeta {
        label: "lbl".to_string(),
        run_id: "run1".to_string(),
        session_key: "sess1".to_string(),
    }
}

#[test]
fn test_build_started_messages_single_headered_message_with_inline_task() {
    let msgs = build_started_messages(&meta(), "abcdef");
    // 1 通・ヘッダ付き。短いので添付なし（JSON 1 本）。task はインライン。
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].content.starts_with("🟢 **subtask started**"));
    assert!(msgs[0].content.contains("run1"));
    assert!(msgs[0].content.contains("sess1"));
    assert!(msgs[0].content.contains("task: abcdef"));
    assert!(!msgs[0].has_attachment());
}

#[test]
fn test_build_started_messages_empty_task_has_no_task_line() {
    let msgs = build_started_messages(&meta(), "");
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].content.starts_with("🟢 **subtask started**"));
    assert!(!msgs[0].content.contains("task:"));
}

/// #293 の性質を維持: 長い task text は part X/N 連投にならず、
/// 「プレビュー + 全文添付」の **1 通**（ヘッダ付き）で出る。1 行目は常にヘッダ。
#[test]
fn test_build_started_messages_long_task_becomes_single_attachment() {
    let long = "x".repeat(5000);
    let msgs = build_started_messages(&meta(), &long);
    assert_eq!(msgs.len(), 1, "連投しない・1 通に畳む");
    let body = &msgs[0];
    assert!(body.has_attachment());
    // 本文は Discord の 1 通上限に収まる。
    assert!(
        body.content.chars().count() <= DISCORD_MESSAGE_LIMIT,
        "preview too long: {}",
        body.content.chars().count()
    );
    assert!(!body.content.starts_with("part 1/"));
    assert!(body.content.starts_with("🟢 **subtask started**"));
    // 添付本体は全文（ヘッダ + メタ + task）をロスなく含む。
    let delivered = body.delivered_text();
    assert!(delivered.contains(&long), "task 全文が添付に入っていない");
    let att = body.attachment.as_ref().unwrap();
    assert_eq!(att.filename, "subtask-started.txt");
    assert_eq!(att.content_type, "text/plain; charset=utf-8");
}

#[test]
fn test_build_terminal_message_variants() {
    let m = build_terminal_message("completed", "r", "s", Some(1234), "done ok");
    assert!(m.contains("✅"));
    assert!(m.contains("completed"));
    assert!(m.contains("1234ms"));
    assert!(m.contains("result: done ok"));

    let f = build_terminal_message("failed", "r", "s", None, "boom");
    assert!(f.contains("❌"));
    assert!(f.contains("duration: -"));
    assert!(f.contains("error: boom"));

    let t = build_terminal_message("timed_out", "r", "s", Some(5), "");
    assert!(t.contains("⏱️"));
    // no detail line when empty
    assert!(!t.contains("error:"));

    let a = build_terminal_message("aborted", "r", "s", Some(5), "cancelled");
    assert!(a.contains("🛑"));
    assert!(a.contains("aborted"));
}

#[test]
fn test_build_terminal_message_truncates_long_detail() {
    let long = "y".repeat(5000);
    let m = build_terminal_message("failed", "r", "s", Some(1), &long);
    assert!(m.chars().count() <= DISCORD_MESSAGE_LIMIT);
}

#[test]
fn test_build_progress_message_truncates_long_message() {
    let long = "x".repeat(10_000);
    let m = build_progress_message("r", "s", &long);
    assert!(m.contains("subtask progress"));
    assert!(m.chars().count() <= DISCORD_MESSAGE_LIMIT);
}

#[test]
fn test_exit_reason_to_status_mapping() {
    assert_eq!(exit_reason_to_status("completed"), "completed");
    assert_eq!(exit_reason_to_status("stopped_by_limit"), "completed");
    assert_eq!(exit_reason_to_status("error"), "failed");
    assert_eq!(exit_reason_to_status("timeout"), "timed_out");
}

// ---- shell result summary ----

#[test]
fn test_summarize_shell_result_preserves_output_unredacted() {
    // covered 経路: stdout は redact せずバイト一致で保持する（新要件 §2 P4）。
    let data = json!({
        "exit_code": 0,
        "stdout": "ok API_KEY=supersecretvalue done",
        "stderr": "",
        "truncated": false,
    });
    let s = summarize_shell_result(&data);
    assert_eq!(s.exit_code, Some(0));
    assert!(!s.truncated);
    let out = s.stdout_summary.unwrap();
    assert_eq!(out, "ok API_KEY=supersecretvalue done");
    assert!(!out.contains("[REDACTED]"), "masking marker leaked: {out}");
    assert!(s.stderr_summary.is_none());
}

#[test]
fn test_summarize_shell_result_does_not_clamp_long_output() {
    // 長い出力は head/tail クランプせず full を保持する（ロスレス）。
    let long = "x".repeat(5000);
    let data = json!({ "exit_code": 1, "stdout": long.clone(), "truncated": true });
    let s = summarize_shell_result(&data);
    assert!(s.truncated);
    let out = s.stdout_summary.unwrap();
    assert_eq!(out.chars().count(), 5000);
    assert_eq!(out, long);
    assert!(
        !out.contains("omitted"),
        "must not insert omission marker: {out}"
    );
}

// ---- tool event formatting ----

#[test]
fn test_build_tool_event_message_preserves_secrets_unredacted() {
    // covered 経路: args/stdout 中のシークレット・webhook URL はそのまま残す。
    let view = ToolEventView {
        event: "tool_call_completed".to_string(),
        tool_name: "execute_shell".to_string(),
        tool_call_id: "c1".to_string(),
        depth: 1,
        status: "completed".to_string(),
        duration_ms: Some(42),
        args_summary: Some("cmd: `echo API_KEY=supersecretvalue`".to_string()),
        result_summary: None,
        exit_code: Some(0),
        stdout_summary: Some(
            "token ghp_0123456789abcdefghij hook https://discord.com/api/webhooks/123/abcdefSECRETtoken"
                .to_string(),
        ),
        stderr_summary: None,
        truncated: false,
        rejection_reason: None,
        max_chars: 1500,
    };
    let msgs = build_tool_event_message(&view);
    assert_eq!(msgs.len(), 1);
    // 短いので従来どおり添付なしのテキスト 1 通（回帰なし）。
    assert!(!msgs[0].has_attachment());
    let m = &msgs[0].content;
    assert!(m.contains("tool_call_completed"));
    assert!(m.contains("execute_shell"));
    assert!(m.contains("exit_code"));
    // unredacted: every secret-like string survives byte-for-byte.
    assert!(
        m.contains("API_KEY=supersecretvalue"),
        "kv secret stripped: {m}"
    );
    assert!(
        m.contains("ghp_0123456789abcdefghij"),
        "prefix secret stripped: {m}"
    );
    assert!(
        m.contains("https://discord.com/api/webhooks/123/abcdefSECRETtoken"),
        "webhook url stripped: {m}"
    );
    // no OpenCrab masking markers anywhere.
    assert!(!m.contains("[REDACTED]"), "REDACTED marker present: {m}");
    assert!(!m.contains("[redacted]"), "redacted marker present: {m}");
}

/// #293: 長い出力は part X/N 連投をやめ、「プレビュー 1 通 + 全文添付」になる。
/// ロスレス性（全文が届くこと）は添付側で維持される。
#[test]
fn test_build_tool_event_message_long_output_becomes_one_attachment() {
    let stdout = "z".repeat(5000);
    let view = ToolEventView {
        event: "tool_call_completed".to_string(),
        tool_name: "execute_shell".to_string(),
        tool_call_id: "c".to_string(),
        depth: 1,
        status: "completed".to_string(),
        stdout_summary: Some(stdout.clone()),
        max_chars: 1500,
        ..Default::default()
    };
    let msgs = build_tool_event_message(&view);
    assert_eq!(msgs.len(), 1, "分割連投しない（送信は 1 回）");
    let m = &msgs[0];
    assert!(m.has_attachment(), "長文は添付になる");
    // 本文は Discord の 1 通上限に収まり、part framing は無い。
    assert!(
        m.content.chars().count() <= DISCORD_MESSAGE_LIMIT,
        "preview too long: {}",
        m.content.chars().count()
    );
    assert!(
        !m.content.starts_with("part 1/"),
        "part framing must be gone"
    );
    // 本文は出だしのプレビュー + 添付の案内。
    assert!(m.content.starts_with("✅ **tool_call_completed**"));
    assert!(
        m.content.contains("full text attached"),
        "no attachment notice: {}",
        m.content
    );
    // 添付本体に全文が入る（head/tail/midpoint の切り捨て無し）。
    let full = m.delivered_text();
    assert!(full.contains(&stdout), "attachment lost stdout");
    assert_eq!(full.matches('z').count(), 5000);
    assert!(!full.contains("[REDACTED]"));
    let att = m.attachment.as_ref().unwrap();
    assert_eq!(att.filename, "tool_call_completed-execute_shell.txt");
    assert!(!att.truncated);
}

/// 添付の中身は**本文と同じ文字列**から作られる。したがって上流でマスク済みの
/// stdout（例: nostr の `nsec` マスク）は添付側にもそのまま効き、添付だけが生の
/// 秘密を運ぶことは起きない。
#[test]
fn test_build_tool_event_message_attachment_carries_upstream_masking() {
    // 上流（crates/nostr の mask_secrets）を通った後の形を模す。
    let masked_line = "secret_key = \"<redacted>\" key nsec1<redacted>\n";
    let stdout = masked_line.repeat(200); // 閾値超え
    let view = ToolEventView {
        event: "tool_call_completed".to_string(),
        tool_name: "nostr_cli".to_string(),
        tool_call_id: "c".to_string(),
        depth: 0,
        status: "completed".to_string(),
        stdout_summary: Some(stdout),
        ..Default::default()
    };
    let msgs = build_tool_event_message(&view);
    let m = &msgs[0];
    assert!(m.has_attachment());
    let full = m.delivered_text();
    assert!(
        !full.contains("nsec1supersecret"),
        "attachment leaked an unmasked nsec"
    );
    assert!(full.contains("nsec1<redacted>"), "mask lost in attachment");
    assert!(
        full.contains("secret_key = \"<redacted>\""),
        "mask lost in attachment"
    );
    // プレビュー側も同じ（本文と添付は同一文字列由来）。
    assert!(!m.content.contains("nsec1supersecret"));
}

/// サイズ上限を超える全文は**送信前に**切り詰め、省略した旨を本文と添付の両方に残す。
#[test]
fn test_build_tool_event_message_truncates_oversized_attachment() {
    let stdout = "q".repeat(opencrab_actions::ATTACHMENT_MAX_BYTES + 4096);
    let view = ToolEventView {
        event: "tool_call_completed".to_string(),
        tool_name: "execute_shell".to_string(),
        tool_call_id: "c".to_string(),
        depth: 0,
        status: "completed".to_string(),
        stdout_summary: Some(stdout),
        ..Default::default()
    };
    let msgs = build_tool_event_message(&view);
    let att = msgs[0].attachment.as_ref().expect("attachment");
    assert!(att.truncated);
    assert!(
        att.data.len() <= opencrab_actions::ATTACHMENT_MAX_BYTES,
        "attachment exceeds cap: {}",
        att.data.len()
    );
    assert!(msgs[0].delivered_text().contains("[truncated]"));
    assert!(
        msgs[0].content.contains("truncated"),
        "本文に省略の明示が無い: {}",
        msgs[0].content
    );
}

#[test]
fn test_build_tool_event_message_surfaces_upstream_truncation() {
    // 上流由来の切り捨ては「完全」と偽らず partial と明示する（AC5）。
    let view = ToolEventView {
        event: "tool_call_completed".to_string(),
        tool_name: "execute_shell".to_string(),
        tool_call_id: "c".to_string(),
        depth: 1,
        status: "completed".to_string(),
        stdout_summary: Some("head".to_string()),
        truncated: true,
        max_chars: 1500,
        ..Default::default()
    };
    let msgs = build_tool_event_message(&view);
    let joined = msgs[0].content.clone();
    assert!(
        joined.contains("partial output"),
        "must mark partial: {joined}"
    );
}

// ---- transport: multipart 送信（#293） ----
//
// 実 Discord へは一切出さない。ローカルの最小 HTTP モックを立てて、
// **何回・どんな Content-Type で・何を送ったか**を検査する。

/// モックが受け取った 1 リクエスト。
#[derive(Clone, Debug)]
struct Recorded {
    content_type: String,
    body: Vec<u8>,
}

/// 依存を増やさない最小の HTTP モック（1 リクエスト = 1 接続、Connection: close）。
struct MockWebhook {
    url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<Recorded>>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockWebhook {
    /// `status` を返すモックを立てる。`delay` は応答前の遅延（遅い相手の模擬）。
    /// `statuses` は 1 リクエスト目, 2 リクエスト目 ... に返すステータス（尽きたら最後を反復）。
    async fn start(statuses: Vec<u16>, delay: Duration) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Recorded>::new()));
        let sink = requests.clone();
        let handle = tokio::spawn(async move {
            let mut n = 0usize;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let sink = sink.clone();
                let status = *statuses.get(n).unwrap_or(statuses.last().unwrap());
                n += 1;
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    // ヘッダ終端まで読む。
                    let head_end = loop {
                        let read = match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&chunk[..read]);
                        if let Some(p) =
                            buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
                        {
                            break p;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let lower = head.to_ascii_lowercase();
                    let header = |name: &str| -> Option<String> {
                        lower.split("\r\n").find_map(|l| {
                            l.strip_prefix(&format!("{name}: "))
                                .map(|v| v.trim().to_string())
                        })
                    };
                    let len: usize = header("content-length")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    while buf.len() < head_end + len {
                        let read = match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&chunk[..read]);
                    }
                    sink.lock().unwrap().push(Recorded {
                        content_type: header("content-type").unwrap_or_default(),
                        body: buf[head_end..].to_vec(),
                    });
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        MockWebhook {
            url: format!("http://{addr}/api/webhooks/1/tok"),
            requests,
            _handle: handle,
        }
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn take(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    /// 指定件数に達するまで待つ（達しなければ panic）。
    async fn wait_for(&self, n: usize, within: Duration) {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if self.count() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {n} request(s), got {}", self.count());
    }
}

/// 閾値超過は **1 回の multipart 送信**になり、分割連投しない。
/// プレビューは payload_json 側、全文は添付側に入る。
#[tokio::test]
async fn long_message_is_one_multipart_request_not_many() {
    let mock = MockWebhook::start(vec![204], Duration::ZERO).await;
    let long = "L".repeat(6000);
    let msg = build_message_with_optional_attachment(&long, "unit-test");
    let tx = spawn_run_worker_with_sink(reqwest::Client::new(), None);
    tx.send(DeliveryBatch {
        url: mock.url.clone(),
        messages: vec![msg.clone()],
    })
    .unwrap();
    mock.wait_for(1, Duration::from_secs(5)).await;
    // 追加の POST が来ないことを確認する猶予。
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(mock.count(), 1, "長文でも送信は 1 回だけ");

    let req = &mock.take()[0];
    assert!(
        req.content_type.starts_with("multipart/form-data"),
        "content-type: {}",
        req.content_type
    );
    let body = String::from_utf8_lossy(&req.body).to_string();
    assert!(body.contains("name=\"payload_json\""), "payload_json 欠落");
    assert!(body.contains("name=\"files[0]\""), "files[0] 欠落");
    assert!(body.contains("filename=\"unit-test.txt\""));
    assert!(body.contains("text/plain; charset=utf-8"));
    // 全文が添付として乗っている。
    assert!(body.contains(&long), "添付本文が全文でない");
    // プレビューは指定長 + 案内文。
    assert!(msg.content.contains("full text attached"));
    assert_eq!(
        msg.content.chars().take_while(|c| *c == 'L').count(),
        opencrab_actions::ATTACHMENT_PREVIEW_CHARS
    );
}

/// 閾値以下は**従来どおり JSON のみ**（添付しない）。回帰テスト。
#[tokio::test]
async fn short_message_stays_plain_json() {
    let mock = MockWebhook::start(vec![204], Duration::ZERO).await;
    let tx = spawn_run_worker_with_sink(reqwest::Client::new(), None);
    tx.send(DeliveryBatch {
        url: mock.url.clone(),
        messages: vec![WebhookMessage::text("hello short")],
    })
    .unwrap();
    mock.wait_for(1, Duration::from_secs(5)).await;
    let req = &mock.take()[0];
    assert_eq!(req.content_type, "application/json");
    assert_eq!(
        String::from_utf8_lossy(&req.body),
        r#"{"content":"hello short"}"#
    );
}

/// 添付本文にも上流のマスクが効いている（本文と添付は同一文字列由来）。
#[tokio::test]
async fn attachment_body_keeps_upstream_masking() {
    let mock = MockWebhook::start(vec![204], Duration::ZERO).await;
    // 上流（crates/nostr の mask_secrets）を通った後の形。
    let masked = "nsec1<redacted> line\n".repeat(300);
    let msg = build_message_with_optional_attachment(&masked, "masked");
    let tx = spawn_run_worker_with_sink(reqwest::Client::new(), None);
    tx.send(DeliveryBatch {
        url: mock.url.clone(),
        messages: vec![msg],
    })
    .unwrap();
    mock.wait_for(1, Duration::from_secs(5)).await;
    let body = String::from_utf8_lossy(&mock.take()[0].body).to_string();
    assert!(body.contains("nsec1<redacted>"));
    assert!(
        !body.contains("nsec1qqq"),
        "添付に生の nsec が乗ってはいけない"
    );
}

/// 添付が 4xx で弾かれたら、添付を落として本文だけを JSON で送り直す。
/// リトライの backoff を挟まないので即座に 2 通目が出る。
#[tokio::test]
async fn rejected_attachment_falls_back_to_preview_only_json() {
    let mock = MockWebhook::start(vec![413, 204], Duration::ZERO).await;
    let msg = build_message_with_optional_attachment(&"Z".repeat(5000), "big");
    let tx = spawn_run_worker_with_sink(reqwest::Client::new(), None);
    tx.send(DeliveryBatch {
        url: mock.url.clone(),
        messages: vec![msg],
    })
    .unwrap();
    mock.wait_for(2, Duration::from_secs(5)).await;
    let reqs = mock.take();
    assert!(reqs[0].content_type.starts_with("multipart/form-data"));
    assert_eq!(reqs[1].content_type, "application/json");
    let fallback = String::from_utf8_lossy(&reqs[1].body).to_string();
    assert!(fallback.contains("full text attached"), "本文が違う");
}

/// **配送は呼び出し元をブロックしない**。相手が遅くても `tx.send` は即座に戻り、
/// 呼び出し元の後続処理はそのまま進む（HTTP は spawn 済み worker の中だけ）。
#[tokio::test]
async fn delivery_never_blocks_the_caller() {
    let slow = Duration::from_millis(600);
    let mock = MockWebhook::start(vec![204], slow).await;
    let msg = build_message_with_optional_attachment(&"S".repeat(5000), "slow");
    let tx = spawn_run_worker_with_sink(reqwest::Client::new(), None);

    let start = std::time::Instant::now();
    tx.send(DeliveryBatch {
        url: mock.url.clone(),
        messages: vec![msg],
    })
    .unwrap();
    // 呼び出し元の「後続処理」。遅い相手を待たずに完了できること。
    let mut work = 0u64;
    for i in 0..1000 {
        work += i;
    }
    assert_eq!(work, 499_500);
    assert!(
        start.elapsed() < slow,
        "呼び出し元が配送に引きずられた: {:?}",
        start.elapsed()
    );
    // それでも配送自体はちゃんと出る。
    mock.wait_for(1, Duration::from_secs(5)).await;
}

// ---- live E2E (env-gated, #[ignore] by default) ----
//
// 実 DB の agent 既定 webhook を使って、空 explicit url がフォールバックして実際の
// Discord webhook へ配送されることを確認する。raw url はコードに置かず、実行時に DB
// から解決して使う（secret はログにも出さない）。
//
// 実行例:
//   OPENCRAB_E2E_DB=data/opencrab.db \
//   OPENCRAB_E2E_AGENT_ID=c56f19e0-... \
//   cargo test -p opencrab-discord -- --ignored --nocapture e2e_empty_url_fallback_delivers
#[tokio::test]
#[ignore = "live: posts to a real Discord webhook; env-gated"]
async fn e2e_empty_url_fallback_delivers() {
    let db_path = std::env::var("OPENCRAB_E2E_DB")
        .expect("set OPENCRAB_E2E_DB to an absolute path to the live opencrab.db");
    let agent_id = std::env::var("OPENCRAB_E2E_AGENT_ID")
        .expect("set OPENCRAB_E2E_AGENT_ID to the agent with a configured default webhook");

    // resolve_subtask_webhook は SELECT のみ（書き込まない）。WAL の live DB に対して
    // read-only open は CannotOpen になり得るため通常 open する。
    let conn = rusqlite::Connection::open(&db_path).expect("open real DB");

    // (1) 空 explicit url → デフォルトへフォールバックして Use になる。
    let empty_args = json!({ "webhook": { "url": "" } });
    let resolved = resolve_subtask_webhook(&conn, &agent_id, "spawn_subtask", &empty_args, None);
    let (url, source) = match resolved {
        WebhookResolution::Use { config, source } => (config.url, source),
        other => panic!(
            "empty url must fall back to a default Use; got {:?}",
            match other {
                WebhookResolution::None => "None",
                WebhookResolution::Disabled { .. } => "Disabled",
                WebhookResolution::Error { .. } => "Error",
                WebhookResolution::Use { .. } => unreachable!(),
            }
        ),
    };
    assert!(
        matches!(
            source,
            WebhookSource::AgentDefault
                | WebhookSource::ToolDefault
                | WebhookSource::GlobalDefault
                | WebhookSource::EnvConfig
        ),
        "fallback source should be a default, got {}",
        source.as_str()
    );
    // raw url は出さない。redacted のみ。
    eprintln!(
        "[e2e] empty url -> fallback source={} url={}",
        source.as_str(),
        redact_webhook_url(&url)
    );

    // 実 Discord webhook へ started lifecycle を 1 通配送する（実 HTTP）。
    let meta = LifecycleMeta {
        label: "E2E empty-url fallback".to_string(),
        run_id: "e2e-empty-url".to_string(),
        session_key: "e2e".to_string(),
    };
    let messages = build_started_messages(
        &meta,
        "E2E: empty explicit webhook url fell back to default (this is a test message).",
    );
    let client = reqwest::Client::new();
    for msg in &messages {
        let resp = client
            .post(&url)
            .json(&build_webhook_body(&msg.content, false))
            .send()
            .await
            .expect("discord webhook POST should complete");
        assert!(
            resp.status().is_success(),
            "discord webhook should accept message: http {}",
            resp.status().as_u16()
        );
    }
    eprintln!(
        "[e2e] delivered {} message(s) to the default webhook",
        messages.len()
    );

    // (2) 非空の不正 url はフォールバックせず Error（strict 維持）。
    let bad_args = json!({ "webhook": { "url": "http://evil.example.com/api/webhooks/1/tok" } });
    let bad = resolve_subtask_webhook(&conn, &agent_id, "spawn_subtask", &bad_args, None);
    match bad {
        WebhookResolution::Error {
            code,
            message,
            source,
        } => {
            assert_eq!(code, "invalid_webhook_url");
            assert_eq!(source, WebhookSource::Explicit);
            assert!(
                !message.contains("evil.example.com"),
                "raw url leaked: {message}"
            );
            eprintln!("[e2e] invalid explicit url -> Error (no fallback): {code}: {message}");
        }
        _ => panic!("non-empty invalid explicit url must Error, not fall back"),
    }
}
