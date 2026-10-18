// D-1085 §12: a lightweight turn keeps the normal tools. A provisional run runs the read-only
// tools at once and waits for its promotion before any other tool; its rows are recorded only
// when it is promoted, after the said's own rows.

/// A gateway that declares one inline operation `gw_act` and records its executions.
struct RecordingOps {
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl opencrab_gateway::GatewayActions for RecordingOps {
    fn definitions(&self) -> Vec<opencrab_gateway::GatewayActionDef> {
        vec![opencrab_gateway::GatewayActionDef {
            name: "gw_act".to_string(),
            description: "a gateway operation".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            class: opencrab_gateway::ToolClass {
                dispatch: opencrab_gateway::DispatchMode::Inline,
                sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                sharing: opencrab_gateway::ToolSharing::ConversationBound,
            },
        }]
    }

    async fn execute(
        &self,
        name: &str,
        _args: &serde_json::Value,
        _ctx: &opencrab_gateway::GatewayCallContext,
    ) -> opencrab_gateway::GatewayActionResult {
        self.calls.lock().unwrap().push(name.to_string());
        opencrab_gateway::GatewayActionResult {
            success: true,
            data: Some(serde_json::json!({"done": true})),
            error: None,
        }
    }
}

fn recording_ops() -> (Arc<dyn opencrab_gateway::GatewayActions>, Arc<Mutex<Vec<String>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let ops = RecordingOps {
        calls: calls.clone(),
    };
    (Arc::new(ops), calls)
}

fn tool_call_message(id: &str, name: &str, args: serde_json::Value) -> Message {
    let mut message = Message::assistant("");
    message.content = None;
    message.tool_calls = Some(vec![opencrab_llm::message::ToolCall {
        id: id.to_string(),
        call_type: "function".to_string(),
        function: opencrab_llm::message::FunctionCall {
            name: name.to_string(),
            arguments: args.to_string(),
        },
    }]);
    message
}

/// Memory search, then the gateway operation, then the final text.
fn search_then_gateway_tool(f: &Fixture) {
    *f.script.lock().unwrap() = vec![
        tool_call_message("c1", "search_memory_index", serde_json::json!({"query": "x"})),
        tool_call_message("c2", "gw_act", serde_json::json!({})),
    ];
    *f.reply.lock().unwrap() = "じゃあね。".to_string();
}

/// Rows after `after`, labelled `speech:<content>`, `call:<tool>` or `result:<tool>`.
fn rows_after(f: &Fixture, after: i64) -> Vec<String> {
    let conn = f.state.db.lock().unwrap();
    opencrab_db::queries::list_session_logs_by_session(&conn, "sess-a")
        .unwrap()
        .into_iter()
        .filter(|row| row.id.is_some_and(|id| id > after))
        .map(|row| {
            let meta: serde_json::Value = row
                .metadata_json
                .as_deref()
                .and_then(|m| serde_json::from_str(m).ok())
                .unwrap_or_default();
            match row.log_type.as_str() {
                "tool_call" => {
                    let calls: serde_json::Value = meta["tool_calls_json"]
                        .as_str()
                        .and_then(|j| serde_json::from_str(j).ok())
                        .unwrap_or_default();
                    format!("call:{}", calls[0]["function"]["name"].as_str().unwrap_or("?"))
                }
                "tool_result" => format!("result:{}", meta["tool_name"].as_str().unwrap_or("?")),
                other => format!("{other}:{}", row.content),
            }
        })
        .collect()
}

async fn wait_for_requests(f: &Fixture, n: usize) {
    for _ in 0..400 {
        if f.requests.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("expected {n} LLM requests, got {}", f.requests.lock().unwrap().len());
}

/// §12: a realtime turn executes a gateway-declared tool and continues to the final text.
#[tokio::test]
async fn realtime_turn_executes_a_gateway_tool() {
    let f = fixture();
    search_then_gateway_tool(&f);
    let (ops, calls) = recording_ops();
    let result = profile_turn_with(&f, TurnModelProfile::Realtime, Some(ops)).await;
    assert_eq!(*calls.lock().unwrap(), ["gw_act"]);
    assert_eq!(result.response, "じゃあね。");
    assert_eq!(f.requests.lock().unwrap().len(), 3);
}

/// §12: before the said the read-only search runs and the gateway tool waits; the promotion runs
/// it once, and the held rows are recorded after the said's rows, in run order.
#[tokio::test]
async fn promoted_provisional_runs_the_gated_tool_once_after_the_said() {
    let f = fixture();
    search_then_gateway_tool(&f);
    let (ops, calls) = recording_ops();
    let hw = high_water(&f);
    let gate = opencrab_core::PromotionGate::new();
    let pending = [log_row("speech", Some("u1"), "SAID-USER", None)];
    let run = provisional_run_with(&f, hw, &pending, gate.clone(), Some(ops));
    let said = async {
        wait_for_requests(&f, 2).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(calls.lock().unwrap().is_empty(), "the gateway tool waits");
        assert_eq!(f.requests.lock().unwrap().len(), 2, "the search ran before the said");
        assert!(rows_after(&f, hw).is_empty(), "nothing recorded before the said");
        {
            let conn = f.state.db.lock().unwrap();
            opencrab_db::queries::insert_session_log(
                &conn,
                &log_row("speech", Some("u1"), "SAID-USER", None),
            )
            .unwrap();
            opencrab_db::queries::insert_session_log(
                &conn,
                &log_row("speech", Some("a1"), "SAID-PRELUDE", None),
            )
            .unwrap();
        }
        gate.promote();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(run, said)
    })
    .await
    .expect("run ends");
    assert_eq!(result.unwrap().response, "じゃあね。");
    assert_eq!(*calls.lock().unwrap(), ["gw_act"]);
    assert_eq!(
        rows_after(&f, hw),
        [
            "speech:SAID-USER",
            "speech:SAID-PRELUDE",
            "call:search_memory_index",
            "result:search_memory_index",
            "call:gw_act",
            "result:gw_act",
        ]
    );
}

/// §12: a mismatched or cancelled provisional (gate aborted) never runs the gateway tool and
/// records nothing.
#[tokio::test]
async fn aborted_provisional_never_runs_the_gated_tool() {
    let f = fixture();
    search_then_gateway_tool(&f);
    let (ops, calls) = recording_ops();
    let hw = high_water(&f);
    let gate = opencrab_core::PromotionGate::new();
    let pending = [log_row("speech", Some("u1"), "SAID-USER", None)];
    let run = provisional_run_with(&f, hw, &pending, gate.clone(), Some(ops));
    let cancel = async {
        wait_for_requests(&f, 2).await;
        gate.abort();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(run, cancel)
    })
    .await
    .expect("run ends");
    assert!(result.is_err());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(calls.lock().unwrap().is_empty());
    assert!(rows_after(&f, hw).is_empty(), "{:?}", rows_after(&f, hw));
}

/// §12: `spawn_subtask` is not read-only: the provisional run stops there until it is decided.
#[tokio::test]
async fn provisional_waits_at_spawn_subtask() {
    let f = fixture();
    *f.script.lock().unwrap() = vec![tool_call_message(
        "c1",
        "spawn_subtask",
        serde_json::json!({"task": "調べて"}),
    )];
    let hw = high_water(&f);
    let gate = opencrab_core::PromotionGate::new();
    let pending = [log_row("speech", Some("u1"), "SAID-USER", None)];
    let run = provisional_run_with(&f, hw, &pending, gate.clone(), None);
    let cancel = async {
        wait_for_requests(&f, 1).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(f.requests.lock().unwrap().len(), 1, "no tool result, no next call");
        let subtasks: i64 = f
            .state
            .db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM sessions WHERE id LIKE 'subtask-%'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(subtasks, 0, "no subtask started");
        gate.abort();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(run, cancel)
    })
    .await
    .expect("run ends");
    assert!(result.is_err());
}
