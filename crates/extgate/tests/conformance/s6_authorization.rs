
#[derive(Clone, Copy)]
enum S6SpeechPath {
    Holding,
    Ordinary,
    LateInbound,
}

struct S6CountingLlm {
    response: Mutex<Option<opencrab_core::ChatResponse>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl opencrab_core::LlmClient for S6CountingLlm {
    async fn chat(
        &self,
        _request: opencrab_core::ChatRequest,
    ) -> anyhow::Result<opencrab_core::ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.response.lock().unwrap().take().unwrap())
    }
}

struct S6CountingExecutor(Arc<AtomicUsize>);

#[async_trait]
impl opencrab_core::ActionExecutor for S6CountingExecutor {
    async fn execute(
        &self,
        _name: &str,
        _args: &serde_json::Value,
    ) -> opencrab_core::ActionResult {
        self.0.fetch_add(1, Ordering::SeqCst);
        opencrab_core::ActionResult {
            success: true,
            data: json!({"ok": true}),
            error: None,
        }
    }

    fn list_tools(&self) -> Vec<opencrab_core::FunctionDefinition> {
        Vec::new()
    }
}

struct S6LateInbound(Mutex<Option<Vec<String>>>);

impl opencrab_core::LiveInboundSource for S6LateInbound {
    fn poll_new_messages(&self) -> Vec<String> {
        self.0.lock().unwrap().take().unwrap_or_default()
    }
}

fn s6_response(path: S6SpeechPath) -> opencrab_core::ChatResponse {
    let (content, tool_calls) = match path {
        S6SpeechPath::Holding => (
            "blocked speech",
            json!([{
                "id": "tool-1",
                "type": "function",
                "function": {"name": "test_tool", "arguments": "{}"}
            }]),
        ),
        S6SpeechPath::Ordinary => ("blocked speech", Value::Null),
        S6SpeechPath::LateInbound => ("blocked speech\nNO_REPLY", Value::Null),
    };
    serde_json::from_value(json!({
        "id": "response-1",
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content,
                "tool_calls": tool_calls
            },
            "finish_reason": null
        }],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0
        },
        "created": 0
    }))
    .unwrap()
}

#[tokio::test]
async fn s6_three_real_continuation_callbacks_leave_speech_and_delivery_ledgers_empty() {
    for mutation in ["revoke", "revision_bump"] {
        for path in [
            S6SpeechPath::Holding,
            S6SpeechPath::Ordinary,
            S6SpeechPath::LateInbound,
        ] {
            let h = Harness::start().await;
            let (_stream, instance_id, binding_id) = ready_pair(&h).await;
            {
                let conn = h.state.db.lock().unwrap();
                opencrab_db::queries::insert_trusted_co_agent(
                    &conn,
                    &opencrab_db::queries::TrustedCoAgentRow {
                        id: "relationship-1".into(),
                        agent_id: "agent-1".into(),
                        co_agent_id: "peer-agent".into(),
                        allowed_actions: None,
                        created_by: "owner".into(),
                        created_at: "2026-01-01T00:00:00Z".into(),
                        relationship_revision: 1,
                        active: true,
                    },
                )
                .unwrap();
            }
            let model_calls = Arc::new(AtomicUsize::new(0));
            let tool_calls = Arc::new(AtomicUsize::new(0));
            let llm = S6CountingLlm {
                response: Mutex::new(Some(s6_response(path))),
                calls: model_calls.clone(),
            };
            let mut engine = opencrab_core::SkillEngine::new(
                Box::new(llm),
                Box::new(S6CountingExecutor(tool_calls.clone())),
                2,
            );
            let check_db = h.state.db.clone();
            let released = Arc::new(AtomicBool::new(false));
            engine.set_authorization_check(Arc::new(move |boundary| {
                let conn = check_db.lock().unwrap();
                let current = opencrab_db::queries::co_agent_relationship_is_current(
                    &conn,
                    "agent-1",
                    "peer-agent",
                    1,
                )
                .unwrap_or(false);
                if current
                    && boundary
                        == opencrab_core::authorization::AuthorizationBoundary::AutomaticContinuation
                    && !released.swap(true, Ordering::SeqCst)
                {
                    match mutation {
                        "revoke" => assert!(opencrab_db::queries::delete_trusted_co_agent(
                            &conn,
                            "agent-1",
                            "peer-agent",
                        )
                        .unwrap()),
                        "revision_bump" => assert!(
                            opencrab_db::queries::bump_trusted_co_agent_revision(
                                &conn,
                                "agent-1",
                                "peer-agent",
                            )
                            .unwrap()
                        ),
                        _ => unreachable!(),
                    }
                }
                current
            }));
            let session_id = session_id_for_binding(&binding_id);
            engine.set_on_continuation_speech({
                let state = h.state.clone();
                let instance_id = instance_id.clone();
                let binding_id = binding_id.clone();
                let session_id = session_id.clone();
                Arc::new(move |speech| {
                    let state = state.clone();
                    let instance_id = instance_id.clone();
                    let binding_id = binding_id.clone();
                    let session_id = session_id.clone();
                    Box::pin(async move {
                        opencrab_extgate::delivery::deliver_intermediate_say(
                            &state,
                            &instance_id,
                            &binding_id,
                            "agent-1",
                            &session_id,
                            &speech,
                            None,
                        )
                        .await
                        .map(|_| ())
                        .map_err(|error| anyhow::anyhow!(error.code.as_str().to_string()))
                    })
                })
            });
            if matches!(path, S6SpeechPath::LateInbound) {
                engine.set_live_inbound(Arc::new(S6LateInbound(Mutex::new(Some(vec![
                    "late message".to_string(),
                ])))));
            }
            assert!(engine.run("system", "input", "test-model").await.is_err());
            assert_eq!(model_calls.load(Ordering::SeqCst), 1, "{mutation}");
            assert_eq!(tool_calls.load(Ordering::SeqCst), 0, "{mutation}");
            let conn = h.state.db.lock().unwrap();
            let deliveries: i64 = conn
                .query_row("SELECT COUNT(*) FROM deliveries", [], |row| row.get(0))
                .unwrap();
            let speech: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!((speech, deliveries), (0, 0), "{mutation}");
        }
    }
}
