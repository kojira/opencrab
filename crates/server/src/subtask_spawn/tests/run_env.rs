// D-1091: a subtask runs in the **same binding** as its parent, so the parent turn's declared
// environment must reach the subtask's `execute_shell` children too.
//
// The seam is the production one: `SystemGatewayActions` carries the run's declaration and
// `spawn_subtask` hands it to the sub-run's `RunRequest`. Without that carry the sub-run starts
// from an empty map and its children see the wrong account, while the parent's own tools see the
// right one — the failure these tests pin.

/// A stub whose first reply calls `execute_shell printenv <NAME>` and whose second reply records
/// the tool result text the engine produced.
struct PrintenvStub {
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl opencrab_llm::traits::LlmProvider for PrintenvStub {
    fn name(&self) -> &str {
        "mock"
    }
    fn sends_max_output_tokens(&self) -> bool {
        false
    }
    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(
        &self,
        request: opencrab_llm::message::ChatRequest,
    ) -> anyhow::Result<opencrab_llm::message::ChatResponse> {
        let tool_result = request.messages.iter().rev().find_map(|m| {
            (m.role == opencrab_llm::message::Role::Tool).then(|| match &m.content {
                Some(opencrab_llm::message::MessageContent::Text(t)) => t.clone(),
                _ => String::new(),
            })
        });
        let message = match tool_result {
            Some(result) => {
                self.seen.lock().unwrap().push(result);
                opencrab_llm::message::Message::assistant("done\nNO_REPLY")
            }
            None => {
                let mut m = opencrab_llm::message::Message::assistant("");
                m.tool_calls = Some(vec![opencrab_llm::message::ToolCall {
                    id: "call-printenv".to_string(),
                    call_type: "function".to_string(),
                    function: opencrab_llm::message::FunctionCall {
                        name: "execute_shell".to_string(),
                        arguments: serde_json::json!({
                            "command": "printenv",
                            "args": [DECLARED_NAME],
                        })
                        .to_string(),
                    },
                }]);
                m
            }
        };
        let finish = if message.tool_calls.is_some() {
            opencrab_llm::message::FinishReason::ToolCalls
        } else {
            opencrab_llm::message::FinishReason::Stop
        };
        Ok(opencrab_llm::message::ChatResponse {
            id: "resp-1".to_string(),
            model: request.model,
            choices: vec![opencrab_llm::message::Choice {
                index: 0,
                message,
                finish_reason: Some(finish),
            }],
            usage: Default::default(),
            created: 0,
        })
    }
}

const DECLARED_NAME: &str = "OPENCRAB_TEST_BINDING_ACCOUNT";

/// Shell enabled with only `printenv` allowed and no inherited variables: the only way the name
/// can appear in the child is the binding declaration.
fn state_with_printenv_stub(seen: Arc<Mutex<Vec<String>>>) -> AppState {
    let state = crate::test_app_state();
    insert_agent_x(&state);
    {
        let mut cfg = state.tools_config.write().unwrap();
        cfg.enabled = true;
        cfg.shell = Some(opencrab_actions::tools::ShellToolConfig {
            enabled: true,
            allowed_commands: vec!["printenv".to_string()],
            inherit_env: false,
            allowed_env_vars: Vec::new(),
            ..Default::default()
        });
    }
    let mut router = opencrab_llm::router::LlmRouter::new();
    router.add_provider(Arc::new(PrintenvStub { seen }));
    state.llm_router.swap(router);
    state
}

/// Spawn one subtask through `SystemGatewayActions` and return the tool-result text its
/// `execute_shell` produced.
async fn subtask_printenv_output(
    parent_run_env: std::collections::BTreeMap<String, String>,
) -> String {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let state = state_with_printenv_stub(seen.clone());
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry()), None)
        .with_run_env(parent_run_env);
    // `execute_shell` is owner-only (#330), and a subtask inherits the parent's caller, so the
    // parent turn runs as owner here and the child's shell call is allowed.
    let parent = GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
        .with_session_id("subtask-env-parent");
    let res = actions
        .execute("spawn_subtask", &json!({ "task": "アカウントを確かめる" }), &parent)
        .await;
    assert!(res.success, "spawn_subtask: {:?}", res.error);
    assert!(
        wait_until(|| !seen.lock().unwrap().is_empty()).await,
        "the subtask never ran execute_shell"
    );
    let first = seen.lock().unwrap().first().cloned().unwrap_or_default();
    first
}

/// The parent turn's binding declaration reaches the subtask's `execute_shell` child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subtask_inherits_the_parent_bindings_run_env() {
    let output = subtask_printenv_output(std::collections::BTreeMap::from([(
        DECLARED_NAME.to_string(),
        "binding-account-1".to_string(),
    )]))
    .await;
    assert!(
        output.contains("binding-account-1"),
        "the subtask child must see the parent binding's declaration, got: {output}"
    );
}

/// An empty parent declaration stays empty: the subtask child sees nothing new.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_undeclared_parent_run_env_stays_empty_in_the_subtask() {
    let output = subtask_printenv_output(std::collections::BTreeMap::new()).await;
    assert!(
        !output.contains("binding-account-1"),
        "an undeclared variable must not appear in the subtask child, got: {output}"
    );
    assert!(
        !output.is_empty(),
        "the subtask's execute_shell must still have run (result text is expected)"
    );
}
