use super::super::*;
use super::common::*;
use crate::traits::CallerIdentity;
use opencrab_gateway::{GatewayActionDef, GatewayActionResult};
use serde_json::json;
use std::sync::Mutex;

// ---- ToolEventSink ----

struct RecordingSink {
    events: Mutex<Vec<(String, String)>>, // (tool_call_id, status)
}
impl ToolEventSink for RecordingSink {
    fn on_event(&self, ev: &ToolEvent<'_>) {
        let status = match ev.status {
            ToolEventStatus::Started => "started",
            ToolEventStatus::Completed => "completed",
            ToolEventStatus::Failed => "failed",
            ToolEventStatus::Rejected => "rejected",
        };
        self.events
            .lock()
            .unwrap()
            .push((ev.tool_call_id.to_string(), status.to_string()));
    }
}

/// owner-only エラーを返す gateway モック（rejected 判定の確認用）。
struct MockGatewayRejecting;
#[async_trait]
impl GatewayActions for MockGatewayRejecting {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        vec![GatewayActionDef {
            name: "rej_action".to_string(),
            class: opencrab_gateway::ToolClass {
                dispatch: opencrab_gateway::DispatchMode::Inline,
                sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                sharing: opencrab_gateway::ToolSharing::AgentBound,
            },
            description: "rej".to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        }]
    }
    async fn execute(
        &self,
        _name: &str,
        _args: &serde_json::Value,
        _ctx: &opencrab_gateway::GatewayCallContext,
    ) -> GatewayActionResult {
        GatewayActionResult {
            success: false,
            data: None,
            error: Some("this action is owner-only".to_string()),
        }
    }
}

#[tokio::test]
async fn test_tool_event_sink_started_then_completed() {
    let (_dir, ctx) = test_context();
    let sink = Arc::new(RecordingSink {
        events: Mutex::new(Vec::new()),
    });
    let executor =
        BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
    let r = executor
        .execute("generate_inner_voice", &json!({"thought": "hi"}))
        .await;
    assert!(r.success);
    let evs = sink.events.lock().unwrap();
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0].1, "started");
    assert_eq!(evs[1].1, "completed");
    // same correlation id for the pair
    assert_eq!(evs[0].0, evs[1].0);
}

#[tokio::test]
async fn test_tool_event_sink_failed_on_unknown() {
    let (_dir, ctx) = test_context();
    let sink = Arc::new(RecordingSink {
        events: Mutex::new(Vec::new()),
    });
    let executor =
        BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
    let _ = executor.execute("nonexistent_tool", &json!({})).await;
    let evs = sink.events.lock().unwrap();
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[1].1, "failed");
}

#[tokio::test]
async fn test_tool_event_sink_rejected_on_permission_error() {
    let (_dir, ctx) = test_context();
    let sink = Arc::new(RecordingSink {
        events: Mutex::new(Vec::new()),
    });
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
        .with_gateway_actions(Arc::new(MockGatewayRejecting))
        .with_tool_event_sink(sink.clone());
    let _ = executor.execute("rej_action", &json!({})).await;
    let evs = sink.events.lock().unwrap();
    assert_eq!(evs[1].1, "rejected");
}

// ---- M1: structured rejection classification ----

/// 構造マーカー接頭辞付きのエラーを返す gateway モック（構造的 rejected 判定用）。
struct MockGatewayStructuredReject;
#[async_trait]
impl GatewayActions for MockGatewayStructuredReject {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        vec![GatewayActionDef {
            name: "sr_action".to_string(),
            class: opencrab_gateway::ToolClass {
                dispatch: opencrab_gateway::DispatchMode::Inline,
                sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                sharing: opencrab_gateway::ToolSharing::AgentBound,
            },
            description: "sr".to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        }]
    }
    async fn execute(
        &self,
        _name: &str,
        _args: &serde_json::Value,
        _ctx: &opencrab_gateway::GatewayCallContext,
    ) -> GatewayActionResult {
        GatewayActionResult {
            success: false,
            data: None,
            // reject() ヘルパが付ける構造マーカーを模す。
            error: Some(format!("{REJECTION_CODE_PREFIX}forbidden_scope: nope")),
        }
    }
}

/// "permission denied" を含む通常の実行失敗を返す gateway モック。
/// これは実行されたが失敗したケースで、rejected に誤分類されてはならない。
struct MockGatewayOrdinaryPermFailure;
#[async_trait]
impl GatewayActions for MockGatewayOrdinaryPermFailure {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        vec![GatewayActionDef {
            name: "perm_fail".to_string(),
            class: opencrab_gateway::ToolClass {
                dispatch: opencrab_gateway::DispatchMode::Inline,
                sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                sharing: opencrab_gateway::ToolSharing::AgentBound,
            },
            description: "pf".to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        }]
    }
    async fn execute(
        &self,
        _name: &str,
        _args: &serde_json::Value,
        _ctx: &opencrab_gateway::GatewayCallContext,
    ) -> GatewayActionResult {
        GatewayActionResult {
            success: false,
            data: None,
            // OS 由来の通常失敗。広い NL 一致なら誤って rejected になる。
            error: Some("write failed: Permission denied (os error 13)".to_string()),
        }
    }
}

#[test]
fn test_is_rejection_structured_marker() {
    assert!(is_rejection(Some(&format!(
        "{REJECTION_CODE_PREFIX}anything at all"
    ))));
}

#[test]
fn test_is_rejection_ignores_ordinary_permission_failures() {
    // 実行されたが失敗した通常エラーは rejected ではない。
    assert!(!is_rejection(Some("Permission denied (os error 13)")));
    assert!(!is_rejection(Some("operation not permitted")));
    assert!(!is_rejection(Some("forbidden by remote host")));
    assert!(!is_rejection(Some("access denied to file")));
}

#[test]
fn test_is_rejection_legacy_domain_markers() {
    // マーカー未付与の owner-only gateway action 等は後方互換で検知する。
    assert!(is_rejection(Some("this action is owner-only")));
    assert!(is_rejection(Some("forbidden_scope: ...")));
    assert!(is_rejection(Some("redacted read requires owner")));
}

#[tokio::test]
async fn test_tool_event_sink_rejected_on_structured_marker() {
    let (_dir, ctx) = test_context();
    let sink = Arc::new(RecordingSink {
        events: Mutex::new(Vec::new()),
    });
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
        .with_gateway_actions(Arc::new(MockGatewayStructuredReject))
        .with_tool_event_sink(sink.clone());
    let _ = executor.execute("sr_action", &json!({})).await;
    let evs = sink.events.lock().unwrap();
    assert_eq!(evs[1].1, "rejected");
}

#[tokio::test]
async fn test_tool_event_sink_ordinary_permission_failure_is_failed_not_rejected() {
    let (_dir, ctx) = test_context();
    let sink = Arc::new(RecordingSink {
        events: Mutex::new(Vec::new()),
    });
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
        .with_gateway_actions(Arc::new(MockGatewayOrdinaryPermFailure))
        .with_tool_event_sink(sink.clone());
    let _ = executor.execute("perm_fail", &json!({})).await;
    let evs = sink.events.lock().unwrap();
    assert_eq!(evs[1].1, "failed", "ordinary failure must not be rejected");
}

// ---- M2: tool_call_id propagation ----

#[tokio::test]
async fn test_execute_with_id_propagates_tool_call_id() {
    let (_dir, ctx) = test_context();
    let sink = Arc::new(RecordingSink {
        events: Mutex::new(Vec::new()),
    });
    let executor =
        BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
    let r = executor
        .execute_with_id(
            "generate_inner_voice",
            &json!({"thought": "hi"}),
            "llm-call-42",
        )
        .await;
    assert!(r.success);
    let evs = sink.events.lock().unwrap();
    assert_eq!(evs.len(), 2);
    // start/terminal の両方が LLM 由来 ID を伝播する。
    assert_eq!(evs[0].0, "llm-call-42");
    assert_eq!(evs[1].0, "llm-call-42");
}

#[tokio::test]
async fn test_execute_without_id_synthesizes_stable_pair() {
    let (_dir, ctx) = test_context();
    let sink = Arc::new(RecordingSink {
        events: Mutex::new(Vec::new()),
    });
    let executor =
        BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
    // id 無し: 合成 UUID だが start/terminal で一致する。
    let _ = executor
        .execute("generate_inner_voice", &json!({"thought": "hi"}))
        .await;
    let evs = sink.events.lock().unwrap();
    assert_eq!(evs.len(), 2);
    assert!(!evs[0].0.is_empty());
    assert_eq!(evs[0].0, evs[1].0);
}

#[tokio::test]
async fn test_no_sink_is_noop() {
    let (_dir, ctx) = test_context();
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
    let r = executor
        .execute("generate_inner_voice", &json!({"thought": "hi"}))
        .await;
    assert!(r.success);
}

// ---- #620: sink は args/result を**そのまま**受け取る（nsec キー名マスクは撤去） ----

/// 各イベントが sink で実際に受け取った args / result を保存する sink。
/// bridge が渡した値そのものを観測する。
struct IoCapturingSink {
    #[allow(clippy::type_complexity)]
    seen: Mutex<Vec<(serde_json::Value, Option<serde_json::Value>)>>,
}
impl ToolEventSink for IoCapturingSink {
    fn on_event(&self, ev: &ToolEvent<'_>) {
        self.seen
            .lock()
            .unwrap()
            .push((ev.args.clone(), ev.result.cloned()));
    }
}

/// #620: args は sink へ**そのまま**（改変せず）渡る。キー名マスク（SECRET_KEYS）は
/// 撤去した。実際には `nsec` を JSON キーに持つ引数を出す producer は皆無なので、
/// この撤去で外部へ出る内容は実運用では変わらない（マスク痕跡 `[redacted]` は付かない）。
#[tokio::test]
async fn test_sink_receives_raw_args_unchanged() {
    let (_dir, ctx) = test_context();
    let sink = Arc::new(IoCapturingSink {
        seen: Mutex::new(Vec::new()),
    });
    let executor =
        BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
    let args = json!({"command": "echo hi", "npub": "npub1ok"});
    let _ = executor.execute("tool_no_secret", &args).await;
    let seen = sink.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "start/terminal の 2 イベント");
    for (a, _) in seen.iter() {
        assert_eq!(*a, args, "args が改変された（sink は生で受け取るはず）");
        assert!(
            !a.to_string().contains("[redacted]"),
            "撤去したはずのマスク痕跡が付いている"
        );
    }
}

fn test_context_with_db(
    caller: CallerIdentity,
) -> (tempfile::TempDir, ActionContext, opencrab_db::Db) {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    let dir = tempfile::TempDir::new().unwrap();
    let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
    let ctx = ActionContext {
        caller,
        agent_id: "agent-1".to_string(),
        agent_name: "Test Agent".to_string(),
        session_id: Some("session-1".to_string()),
        db: db.clone(),
        workspace: std::sync::Arc::new(ws),
        last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
        model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
        current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
        runtime_info: std::sync::Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
            default_model: "mock:test-model".to_string(),
            active_model: None,
            available_providers: vec!["mock".to_string()],
            gateway: "test".to_string(),
        })),
    };
    (dir, ctx, db)
}

fn memory_session_count(db: &opencrab_db::Db) -> i64 {
    let conn = db.lock().unwrap();
    conn.query_row("SELECT COUNT(*) FROM memory_sessions", [], |r| r.get(0))
        .unwrap()
}

/// 載せ替え工程 5-b: 成功 / 失敗 / refused が tool_logs 1 行になり、
/// 返り値と memory_sessions は変わらない。
#[tokio::test]
async fn tool_logs_records_done_failed_refused() {
    let (_dir, ctx, db) = test_context_with_db(CallerIdentity::Owner);
    let before = memory_session_count(&db);
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);

    let done = executor
        .execute("generate_inner_voice", &json!({"thought": "hi"}))
        .await;
    assert!(done.success, "成功経路の返り値は不変: {done:?}");

    let failed = executor.execute("nonexistent_tool", &json!({"x": 1})).await;
    assert!(!failed.success);
    assert!(
        failed.error.as_deref().unwrap().contains("Unknown action"),
        "失敗の返り値は不変: {failed:?}"
    );

    let rows = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_tool_logs(&conn, "agent-1", 20).unwrap()
    };
    assert_eq!(rows.len(), 2, "1 実行 1 行");
    let done_row = rows
        .iter()
        .find(|r| r.tool_name == "generate_inner_voice")
        .expect("done 行");
    assert_eq!(done_row.outcome, "done");
    assert_eq!(done_row.session_id.as_deref(), Some("session-1"));
    assert_eq!(done_row.agent_id, "agent-1");
    assert!(done_row.args_json.contains("thought"));
    assert!(done_row.latency_ms.is_some());
    assert!(done_row.started_at.is_some());

    let failed_row = rows
        .iter()
        .find(|r| r.tool_name == "nonexistent_tool")
        .expect("failed 行");
    assert_eq!(failed_row.outcome, "failed");
    assert!(
        failed_row.result_text.contains("Unknown action"),
        "失敗も 1 行: {}",
        failed_row.result_text
    );

    let after = memory_session_count(&db);
    assert!(
        after >= before,
        "memory_sessions は減らさない（既存記録は不変）: before={before} after={after}"
    );
    let inner_voice = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, "session-1")
            .unwrap()
            .into_iter()
            .filter(|l| l.log_type == "inner_voice")
            .count()
    };
    assert_eq!(
        inner_voice, 1,
        "generate_inner_voice の既存 session_logs 記録は残る"
    );

    let (_dir2, ctx2, db2) = test_context_with_db(CallerIdentity::Agent);
    let before2 = memory_session_count(&db2);
    let exec2 = BridgedExecutor::new(ActionDispatcher::new(), ctx2);
    let refused = exec2
        .execute("execute_shell", &json!({"command": "echo hi"}))
        .await;
    assert!(!refused.success);
    assert!(
        is_rejection(refused.error.as_deref()),
        "refused の返り値は不変: {refused:?}"
    );
    let rows2 = {
        let conn = db2.lock().unwrap();
        opencrab_db::queries::list_tool_logs(&conn, "agent-1", 20).unwrap()
    };
    assert_eq!(rows2.len(), 1);
    assert_eq!(rows2[0].outcome, "refused");
    assert_eq!(rows2[0].tool_name, "execute_shell");
    assert_eq!(rows2[0].session_id.as_deref(), Some("session-1"));
    assert_eq!(memory_session_count(&db2), before2);
}

#[tokio::test]
async fn tool_logs_writes_when_sink_is_present() {
    let (_dir, ctx, db) = test_context_with_db(CallerIdentity::Owner);
    let sink = Arc::new(RecordingSink {
        events: Mutex::new(Vec::new()),
    });
    let executor =
        BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
    let r = executor
        .execute("generate_inner_voice", &json!({"thought": "hi"}))
        .await;
    assert!(r.success);
    let evs = sink.events.lock().unwrap();
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0].1, "started");
    assert_eq!(evs[1].1, "completed");
    let rows = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_tool_logs(&conn, "agent-1", 20).unwrap()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "done");
}
