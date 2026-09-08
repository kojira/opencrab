//! DESIGN-WEBGATE §7.4: 新規会話作成の router / request / TX / disconnected。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tower::ServiceExt;
use uuid::Uuid;

/// #829: 実ゲートウェイ（UDS）を起こす bind 系テストと、プロセスグローバルな
/// `set_binding_tx_fail` を触るテストは**並列で走らせない**。並列だと gateway 同士の
/// リソース競合や、fail 注入のグローバル状態が他テストへ漏れて hang / flake する
/// （単一スレッドでは 12/12 安定）。ガードは await を跨いで保持するため、std の Mutex
/// （`await_holding_lock` に触れ、multi-thread runtime では危険）ではなく非同期用の
/// `tokio::sync::Mutex` を使う。poison も無い。
static BIND_TESTS_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn bind_serial() -> tokio::sync::MutexGuard<'static, ()> {
    BIND_TESTS_SERIAL.lock().await
}

const AGENT: &str = "webagent";
const CONFIG_B64: &str = "e30=";
const CONFIG_DIGEST: &str = "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
const INSTANCE: &str = "11111111-1111-4111-8111-111111111111";

fn seed_agent_instance(conn: &Connection) {
    opencrab_db::queries::upsert_agent(
        conn,
        &opencrab_db::queries::AgentRow {
            agent_id: AGENT.into(),
            name: AGENT.into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "p".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    let subject: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = ?1",
            [AGENT],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO gate_instances
         (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
         VALUES (?1, 'web', ?2, 1, 1, ?3, ?4, 1, 1)",
        rusqlite::params![INSTANCE, subject, CONFIG_B64, CONFIG_DIGEST],
    )
    .unwrap();
}

fn app_from_conn(
    conn: Connection,
) -> (
    axum::Router,
    Arc<opencrab_extgate::ExtgateState>,
    opencrab_server::AppState,
) {
    let db = opencrab_db::Db::from_connection(conn);
    let extgate = Arc::new(opencrab_extgate::ExtgateState::new(
        db.clone(),
        opencrab_extgate::OperatorToken::from_bytes("test-token"),
    ));
    let state = opencrab_server::AppState {
        db,
        llm_router: opencrab_server::SharedLlmRouter::new(opencrab_llm::router::LlmRouter::new()),
        llm_config: std::sync::Arc::new(toml::from_str("default_provider = \"\"").unwrap()),
        subtask_auto_dispatch: true,
        voice_config: std::sync::Arc::new(Default::default()),
        voice_runtime: std::sync::Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir().to_string_lossy().to_string(),
        #[cfg(feature = "nostr")]
        nostr_master_key: None,
        default_model: "mock:test".to_string(),
        tools_config: std::sync::Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        )),
        compaction_ratio: 0.5,
        typed_history_enabled: false,
        typed_history_drop_directive: false,
        evaluator: opencrab_server::config::EvaluatorConfig::default(),
        skill_consolidation: opencrab_server::config::SkillConsolidationConfig::default(),
        category_maintenance: opencrab_server::config::CategoryMaintenanceConfig::default(),
        memory_organize: opencrab_server::config::MemoryOrganizeConfig::default(),
        memory_declare: opencrab_server::config::MemoryDeclareConfig::default(),
        memory_condense: opencrab_server::config::MemoryCondenseConfig::default(),
        loop_restart_enabled: false,
        index_build_inflight: std::sync::Arc::new(dashmap::DashMap::new()),
        intake: std::sync::Arc::new(Default::default()),
        intake_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        mcp_manager: None,
        gateways: std::sync::Arc::new(opencrab_actions::AgentGatewayRegistry::new()),
        subtask_registries: std::sync::Arc::new(
            opencrab_server::subtask_registries::SubtaskRegistries::new(),
        ),
        session_locks: std::sync::Arc::new(opencrab_actions::SessionLocks::new()),
        subtask_notifiers: std::sync::Arc::new(dashmap::DashMap::new()),
        subtask_lifecycle_notifier: std::sync::Arc::new(std::sync::Mutex::new(None)),
        default_subtask_webhook: None,
        heartbeat_limits: Default::default(),
        scheduler_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        heartbeat_config_rx: opencrab_server::disconnected_heartbeat_config_rx(Default::default()),
        timed_fire_router: std::sync::Arc::new(opencrab_actions::TimedFireRouter::new()),
        progress_debounce: std::sync::Arc::new(
            opencrab_server::subtask_registries::ProgressDebounce::new(),
        ),
    };
    let app = opencrab_server::create_router_with_gate(state.clone(), extgate.clone());
    (app, extgate, state)
}

async fn call(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

fn create_req(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/api/agents/{AGENT}/web-conversations"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn counts(conn: &Connection) -> (i64, i64, i64) {
    let sessions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE id LIKE 'extgate-%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let members: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_sessions", [], |r| r.get(0))
        .unwrap();
    let bindings: i64 = conn
        .query_row("SELECT COUNT(*) FROM gate_bindings", [], |r| r.get(0))
        .unwrap();
    (sessions, members, bindings)
}

#[test]
fn router_inventory_has_exactly_one_web_conversation_create() {
    let routes = opencrab_server::production_route_inventory();
    let hits: Vec<_> = routes
        .iter()
        .filter(|r| r.path.contains("web-conversations"))
        .collect();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].path, "/api/agents/{agent_id}/web-conversations");
    assert_eq!(hits[0].methods, vec!["POST".to_string()]);
}

#[test]
fn binding_insert_only_lives_in_create_gate_binding_in_tx() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut hits = Vec::new();
    for dir in ["db/src", "extgate/src", "server/src"] {
        let path = root.join(dir);
        let mut files = Vec::new();
        walk_rs(&path, &mut files);
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            let prod = text.split("#[cfg(test)]").next().unwrap_or(&text);
            if prod.contains("INSERT INTO gate_bindings") {
                hits.push(file.display().to_string());
            }
        }
    }
    assert_eq!(
        hits,
        vec![root
            .join("db/src/queries/gate_binding.rs")
            .display()
            .to_string()],
        "{hits:?}"
    );
}

fn walk_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("tests") {
                continue;
            }
            walk_rs(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.contains("test") {
                out.push(path);
            }
        }
    }
}

#[tokio::test]
async fn empty_body_and_name_succeed_as_202_without_live_gateway() {
    let _bind_serial = bind_serial().await;
    let conn = opencrab_db::init_memory().unwrap();
    seed_agent_instance(&conn);
    let (app, _, _) = app_from_conn(conn);
    let (st, body) = call(app.clone(), create_req("{}")).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["state"], "provisioning");
    assert!(body["name"].is_null());
    assert_eq!(
        body["session_id"],
        format!("web-{AGENT}-{}", body["conversation_id"].as_str().unwrap())
    );
    let cid = body["conversation_id"].as_str().unwrap();
    assert_eq!(cid, cid.to_lowercase());
    Uuid::parse_str(cid).unwrap();
    let expected_binding = Uuid::new_v5(
        &Uuid::NAMESPACE_DNS,
        format!(
            "opencrab:web:binding:{}",
            body["session_id"].as_str().unwrap()
        )
        .as_bytes(),
    )
    .to_string();
    assert_eq!(body["binding_id"], expected_binding);

    let (st, named) = call(app, create_req(r#"{"name":"  Dinner  "}"#)).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{named}");
    assert_eq!(named["name"], "Dinner");
}

#[tokio::test]
async fn caller_specified_ids_are_rejected() {
    let _bind_serial = bind_serial().await;
    let conn = opencrab_db::init_memory().unwrap();
    seed_agent_instance(&conn);
    let (app, _, _) = app_from_conn(conn);
    for body in [
        r#"{"conversation_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}"#,
        r#"{"session_id":"web-x"}"#,
        r#"{"binding_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"}"#,
        r#"{"instance_id":"11111111-1111-4111-8111-111111111111"}"#,
        r#"{"address":"web-x"}"#,
    ] {
        let (st, v) = call(app.clone(), create_req(body)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body} {v}");
    }
}

#[tokio::test]
async fn name_newline_and_over_100_scalars_are_400() {
    let _bind_serial = bind_serial().await;
    let conn = opencrab_db::init_memory().unwrap();
    seed_agent_instance(&conn);
    let (app, _, _) = app_from_conn(conn);
    let (st, _) = call(app.clone(), create_req("{\"name\":\"a\\nb\"}")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let long: String = "あ".repeat(101);
    let (st, _) = call(app.clone(), create_req(&json!({"name": long}).to_string())).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let ok100: String = "あ".repeat(100);
    let (st, body) = call(app, create_req(&json!({"name": ok100}).to_string())).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
}

#[tokio::test]
async fn missing_or_duplicate_instance_is_409_write_zero() {
    let _bind_serial = bind_serial().await;
    let conn = opencrab_db::init_memory().unwrap();
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: "alone".into(),
            name: "alone".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "p".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    let (app, _, _) = app_from_conn(conn);
    let req = Request::builder()
        .method("POST")
        .uri("/api/agents/alone/web-conversations")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (st, body) = call(app, req).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["error"], "web_instance_unavailable");
}

#[tokio::test]
async fn tx_failure_injection_leaves_zero_writes() {
    let _bind_serial = bind_serial().await;
    let conn = opencrab_db::init_memory().unwrap();
    seed_agent_instance(&conn);
    let db_path_counts = {
        let (app, _, state) = app_from_conn(conn);
        for step in [
            opencrab_db::queries::FAIL_SESSION,
            opencrab_db::queries::FAIL_MEMBERSHIP,
            opencrab_db::queries::FAIL_BINDING,
            opencrab_db::queries::FAIL_NAME,
            opencrab_db::queries::FAIL_COMMIT,
        ] {
            opencrab_db::queries::set_binding_tx_fail(step);
            let body = if step == opencrab_db::queries::FAIL_NAME {
                r#"{"name":"Named"}"#
            } else {
                "{}"
            };
            let (st, v) = call(app.clone(), create_req(body)).await;
            assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "step {step} {v}");
            let guard = state.db.lock().unwrap();
            assert_eq!(counts(&guard), (0, 0, 0), "step {step}");
        }
        opencrab_db::queries::set_binding_tx_fail(opencrab_db::queries::FAIL_NONE);
        let (st, v) = call(app, create_req("{}")).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        let guard = state.db.lock().unwrap();
        counts(&guard)
    };
    assert_eq!(db_path_counts, (1, 1, 1));
}

#[tokio::test]
async fn admin_put_and_create_share_one_store_command() {
    let _bind_serial = bind_serial().await;
    let conn = opencrab_db::init_memory().unwrap();
    seed_agent_instance(&conn);
    let (app, _, state) = app_from_conn(conn);
    let (st, created) = call(app.clone(), create_req("{}")).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{created}");
    let (st, put) = call(
        app,
        Request::builder()
            .method("PUT")
            .uri("/api/gate-bindings/22222222-2222-4222-8222-222222222222")
            .header("authorization", "Bearer test-token")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"instance_id": INSTANCE, "address": "web-admin-addr"}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{put}");
    let guard = state.db.lock().unwrap();
    let n: i64 = guard
        .query_row("SELECT COUNT(*) FROM gate_bindings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn disconnected_create_is_202_and_detail_is_not_ready() {
    let _bind_serial = bind_serial().await;
    let conn = opencrab_db::init_memory().unwrap();
    seed_agent_instance(&conn);
    let (app, _, _) = app_from_conn(conn);
    let (st, created) = call(app.clone(), create_req("{}")).await;
    assert_eq!(st, StatusCode::ACCEPTED);
    let session = created["session_id"].as_str().unwrap();
    let (st, detail) = call(
        app,
        Request::builder()
            .uri(format!("/api/sessions/{session}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{detail}");
    assert_eq!(detail["gateway_bound"], true);
    assert_eq!(detail["web_binding_state"], "unavailable");
}

async fn write_frame(s: &mut UnixStream, v: &Value) {
    let mut buf = serde_json::to_vec(v).unwrap();
    buf.push(b'\n');
    s.write_all(&buf).await.unwrap();
}

async fn read_frame(s: &mut UnixStream) -> Value {
    let mut buf = Vec::new();
    loop {
        let mut b = [0u8; 1];
        s.read_exact(&mut b).await.expect("read");
        if b[0] == b'\n' {
            break;
        }
        buf.push(b[0]);
    }
    serde_json::from_slice(&buf).unwrap()
}

#[tokio::test]
async fn live_gateway_create_returns_201_ready() {
    let _bind_serial = bind_serial().await;
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gate.sock");
    let conn = opencrab_db::init_memory().unwrap();
    seed_agent_instance(&conn);
    let (app, extgate, state) = app_from_conn(conn);
    let listen = extgate.clone();
    let runtime = state.clone();
    let path = sock.clone();
    tokio::spawn(async move {
        let _ = opencrab_extgate::serve_uds(
            listen,
            runtime,
            opencrab_extgate::resolve_caller_identity_with_owner,
            path,
        )
        .await;
    });
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut gw = UnixStream::connect(&sock).await.expect("connect");
    write_frame(
        &mut gw,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": INSTANCE,
            "revision": 1,
            "config_digest": CONFIG_DIGEST,
        }),
    )
    .await;
    let hello_ok = read_frame(&mut gw).await;
    assert_eq!(hello_ok["m"], "ok", "{hello_ok}");

    let ack = tokio::spawn(async move {
        let bind = read_frame(&mut gw).await;
        assert_eq!(bind["m"], "bind", "{bind}");
        let id = bind["id"].as_str().unwrap().to_string();
        write_frame(&mut gw, &json!({"id": id, "m": "ok"})).await;
        gw
    });

    let (st, body) = call(app.clone(), create_req(r#"{"name":"Live"}"#)).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "ready");
    assert_eq!(body["name"], "Live");
    let _ = ack.await.unwrap();

    let session = body["session_id"].as_str().unwrap();
    let (st, detail) = call(
        app,
        Request::builder()
            .uri(format!("/api/sessions/{session}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{detail}");
    assert_eq!(detail["web_binding_state"], "ready");
    let guard = state.db.lock().unwrap();
    assert_eq!(counts(&guard), (1, 1, 1));
}

#[tokio::test]
async fn socket_close_during_bind_keeps_binding_and_returns_202() {
    let _bind_serial = bind_serial().await;
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gate.sock");
    let conn = opencrab_db::init_memory().unwrap();
    seed_agent_instance(&conn);
    let (app, extgate, state) = app_from_conn(conn);
    let listen = extgate.clone();
    let runtime = state.clone();
    let path = sock.clone();
    tokio::spawn(async move {
        let _ = opencrab_extgate::serve_uds(
            listen,
            runtime,
            opencrab_extgate::resolve_caller_identity_with_owner,
            path,
        )
        .await;
    });
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut gw = UnixStream::connect(&sock).await.expect("connect");
    write_frame(
        &mut gw,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": INSTANCE,
            "revision": 1,
            "config_digest": CONFIG_DIGEST,
        }),
    )
    .await;
    let hello_ok = read_frame(&mut gw).await;
    assert_eq!(hello_ok["m"], "ok");
    let closer = tokio::spawn(async move {
        let bind = read_frame(&mut gw).await;
        assert_eq!(bind["m"], "bind");
        drop(gw);
    });
    let (st, body) = call(app, create_req("{}")).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["state"], "provisioning");
    let _ = closer.await;
    let guard = state.db.lock().unwrap();
    assert_eq!(counts(&guard), (1, 1, 1));
}

#[tokio::test]
async fn race_barriers_keep_single_binding_and_single_bind() {
    let _bind_serial = bind_serial().await;
    opencrab_extgate::race::disarm_all();
    opencrab_extgate::race::arm("after_commit");
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gate.sock");
    let conn = opencrab_db::init_memory().unwrap();
    seed_agent_instance(&conn);
    let (app, extgate, state) = app_from_conn(conn);
    let listen = extgate.clone();
    let runtime = state.clone();
    let path = sock.clone();
    tokio::spawn(async move {
        let _ = opencrab_extgate::serve_uds(
            listen,
            runtime,
            opencrab_extgate::resolve_caller_identity_with_owner,
            path,
        )
        .await;
    });
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut gw = UnixStream::connect(&sock).await.expect("connect");
    write_frame(
        &mut gw,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": INSTANCE,
            "revision": 1,
            "config_digest": CONFIG_DIGEST,
        }),
    )
    .await;
    assert_eq!(read_frame(&mut gw).await["m"], "ok");

    let bind_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let binds = bind_count.clone();
    let acker = tokio::spawn(async move {
        let bind = read_frame(&mut gw).await;
        assert_eq!(bind["m"], "bind", "{bind}");
        binds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = bind["id"].as_str().unwrap().to_string();
        write_frame(&mut gw, &json!({"id": id, "m": "ok"})).await;
        let extra = tokio::time::timeout(Duration::from_millis(200), read_frame(&mut gw)).await;
        (extra.is_err(), gw)
    });

    let create = tokio::spawn(call(app, create_req("{}")));
    let parked = tokio::task::spawn_blocking(|| {
        opencrab_extgate::race::wait_parked("after_commit", Duration::from_secs(5))
    })
    .await
    .unwrap();
    assert!(parked, "after_commit");
    {
        let guard = state.db.lock().unwrap();
        assert_eq!(counts(&guard), (1, 1, 1));
    }
    {
        let reg = extgate.lock_registry().unwrap();
        let live = reg.get(INSTANCE).expect("live");
        assert!(live.pending.is_empty());
        assert!(live.acknowledged.is_empty());
    }

    opencrab_extgate::race::arm("after_pending");
    opencrab_extgate::race::release("after_commit");
    let parked = tokio::task::spawn_blocking(|| {
        opencrab_extgate::race::wait_parked("after_pending", Duration::from_secs(5))
    })
    .await
    .unwrap();
    assert!(parked, "after_pending");
    {
        let guard = state.db.lock().unwrap();
        assert_eq!(counts(&guard), (1, 1, 1));
    }
    {
        let reg = extgate.lock_registry().unwrap();
        let live = reg.get(INSTANCE).expect("live");
        assert_eq!(live.pending.len(), 1);
        assert!(live.acknowledged.is_empty());
    }
    assert_eq!(bind_count.load(std::sync::atomic::Ordering::SeqCst), 0);

    opencrab_extgate::race::arm("before_http_ready");
    opencrab_extgate::race::release("after_pending");
    let parked = tokio::task::spawn_blocking(|| {
        opencrab_extgate::race::wait_parked("before_http_ready", Duration::from_secs(5))
    })
    .await
    .unwrap();
    assert!(parked, "before_http_ready");
    {
        let guard = state.db.lock().unwrap();
        assert_eq!(counts(&guard), (1, 1, 1));
    }
    {
        let reg = extgate.lock_registry().unwrap();
        let live = reg.get(INSTANCE).expect("live");
        assert_eq!(live.acknowledged.len(), 1);
    }

    opencrab_extgate::race::release("before_http_ready");
    let (st, body) = create.await.unwrap();
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "ready");
    let (no_extra, _gw) = acker.await.unwrap();
    assert!(no_extra, "dynamic bind must be exact 1");
    assert_eq!(bind_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    let guard = state.db.lock().unwrap();
    assert_eq!(counts(&guard), (1, 1, 1));
    opencrab_extgate::race::disarm_all();
}
