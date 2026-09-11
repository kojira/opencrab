use super::*;

fn action_context(
    caller: opencrab_actions::CallerIdentity,
    fixture: &str,
) -> opencrab_actions::ActionContext {
    let root = fixture_workspace("tools");
    fs::create_dir_all(&root).expect("create baseline tool workspace");
    if fixture == "workspace_file" {
        fs::create_dir_all(root.join("captured")).expect("create captured fixture directory");
        fs::write(root.join("captured/file.txt"), b"baseline")
            .expect("write captured fixture file");
    }
    let workspace = opencrab_core::workspace::Workspace::from_root(root)
        .expect("baseline tool workspace must be valid");
    let db = opencrab_db::Db::memory().expect("in-memory baseline DB");
    {
        let conn = db.lock().expect("lock baseline tool DB");
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: AGENT_ID.to_string(),
                name: "Baseline Agent".to_string(),
                job_title: Some("Compatibility Probe".to_string()),
                organization: Some("opencrab".to_string()),
                image_url: None,
                persona_name: "Baseline".to_string(),
                personality: Some("deterministic".to_string()),
                instructions: "baseline instructions".to_string(),
                heartbeat_instructions: "baseline heartbeat".to_string(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .expect("seed baseline tool agent");
        opencrab_db::queries::insert_skill(
            &conn,
            &opencrab_db::queries::SkillRow {
                id: "baseline-seed-skill".to_string(),
                agent_id: AGENT_ID.to_string(),
                name: "Seed Skill".to_string(),
                description: "seed skill".to_string(),
                situation_pattern: "baseline".to_string(),
                guidance: "preserve behavior".to_string(),
                source_type: "baseline".to_string(),
                source_context: None,
                file_path: None,
                effectiveness: None,
                usage_count: 0,
                is_active: true,
                permission: "private".to_string(),
                archived: false,
                created_caller: Some("owner".to_string()),
                agent_visible: false,
            },
        )
        .expect("seed baseline tool skill");
        for content in ["baseline memory first", "baseline memory second"] {
            opencrab_db::queries::insert_session_log(
                &conn,
                &opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: AGENT_ID.to_string(),
                    session_id: SESSION_ID.to_string(),
                    log_type: "message".to_string(),
                    content: content.to_string(),
                    speaker_id: Some(AGENT_ID.to_string()),
                    turn_number: Some(1),
                    metadata_json: None,
                    created_at: Some("2026-01-01T00:00:00Z".to_string()),
                },
            )
            .expect("seed baseline tool history");
        }
        conn.execute(
            "UPDATE memory_sessions SET created_at = '2026-01-01T00:00:00Z' WHERE agent_id = ?1 AND session_id = ?2",
            rusqlite::params![AGENT_ID, SESSION_ID],
        )
        .expect("fix baseline history clock input");
        let node = |id: &str,
                    node_type: &str,
                    source_type: &str,
                    title: &str,
                    short_id: &str,
                    start: Option<i64>,
                    end: Option<i64>,
                    keywords_json: &str| opencrab_db::queries::IndexNodeRow {
            id: id.to_string(),
            agent_id: AGENT_ID.to_string(),
            parent_id: None,
            node_type: node_type.to_string(),
            source_type: source_type.to_string(),
            title: title.to_string(),
            summary: format!("{title} summary"),
            start_log_id: start,
            end_log_id: end,
            source_session_id: None,
            date_from: Some("2026-01-01".to_string()),
            date_to: Some("2026-01-01".to_string()),
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: Some(short_id.to_string()),
            keywords_json: keywords_json.to_string(),
            summary_refreshed_at: None,
        };
        for n in [
            node(
                "baseline-topic",
                "topic",
                "session_log",
                "Baseline Topic",
                "t1",
                Some(1),
                Some(2),
                "[]",
            ),
            node(
                "baseline-unit-source",
                "unit",
                "declared",
                "Baseline Unit Source",
                "u1",
                Some(1),
                Some(2),
                "[]",
            ),
            node(
                "baseline-unit-retract",
                "unit",
                "declared",
                "Baseline Unit Retract",
                "u2",
                Some(1),
                Some(1),
                "[]",
            ),
            node(
                "baseline-core-update",
                "meta",
                "condensed",
                "Baseline Core Update",
                "m1",
                Some(1),
                Some(2),
                "[\"u1\"]",
            ),
            node(
                "baseline-core-retract",
                "meta",
                "condensed",
                "Baseline Core Retract",
                "m2",
                Some(1),
                Some(2),
                "[\"u1\"]",
            ),
            node(
                "baseline-tag",
                "category",
                "category",
                "Baseline Tag",
                "c1",
                None,
                None,
                "[]",
            ),
            node(
                "baseline-merge-from",
                "category",
                "category",
                "Merge From",
                "c2",
                None,
                None,
                "[]",
            ),
            node(
                "baseline-merge-into",
                "category",
                "category",
                "Merge Into",
                "c3",
                None,
                None,
                "[]",
            ),
        ] {
            opencrab_db::queries::insert_index_node(&conn, &n)
                .expect("seed baseline tool memory node");
        }
        if fixture == "active_task" {
            opencrab_db::queries::insert_task_ledger(
                &conn,
                AGENT_ID,
                TOOL_SESSION_ID,
                "fixture task",
                Some("fixture contract"),
            )
            .expect("seed active task fixture");
        }
        if fixture == "active_schedule" {
            opencrab_db::queries::insert_agent_schedule(
                &conn,
                &opencrab_db::queries::AgentScheduleRow {
                    id: None,
                    agent_id: AGENT_ID.to_string(),
                    session_id: TOOL_SESSION_ID.to_string(),
                    cron_expr: "0 9 * * *".to_string(),
                    timezone: "UTC".to_string(),
                    message: "fixture schedule".to_string(),
                    enabled: true,
                    anchor_at: None,
                    last_fired_at: None,
                },
            )
            .expect("seed active schedule fixture");
        }
        if fixture == "llm_metrics" {
            opencrab_db::queries::insert_llm_metrics(
                &conn,
                &opencrab_db::queries::LlmMetricsRow {
                    id: "baseline-metrics".to_string(),
                    agent_id: AGENT_ID.to_string(),
                    session_id: Some(TOOL_SESSION_ID.to_string()),
                    timestamp: "2026-01-01T00:00:00Z".to_string(),
                    provider: "baseline".to_string(),
                    model: "baseline:model".to_string(),
                    purpose: "baseline".to_string(),
                    task_type: None,
                    complexity: None,
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    estimated_cost_usd: 0.0,
                    latency_ms: 1,
                    time_to_first_token_ms: None,
                },
            )
            .expect("seed LLM metrics fixture");
        }
        if fixture == "allowed_command" {
            opencrab_db::queries::add_agent_allowed_command(
                &conn,
                AGENT_ID,
                "baseline_cmd",
                "baseline",
            )
            .expect("seed allowed command fixture");
        }
        if fixture == "archived_skill" {
            opencrab_db::queries::archive_skill(&conn, "baseline-seed-skill", true)
                .expect("seed archived skill fixture");
        }
        if fixture == "tagged_topic" {
            opencrab_db::queries::assign_topic_to_category(
                &conn,
                AGENT_ID,
                "baseline-topic",
                "baseline-tag",
                "2026-01-01T00:00:00Z",
            )
            .expect("seed tagged topic fixture");
        }
    }
    opencrab_actions::ActionContext {
        agent_id: AGENT_ID.to_string(),
        agent_name: "Baseline Agent".to_string(),
        session_id: Some(TOOL_SESSION_ID.to_string()),
        db,
        workspace: Arc::new(workspace),
        last_metrics_id: Arc::new(std::sync::Mutex::new(
            (fixture == "llm_metrics").then(|| "baseline-metrics".to_string()),
        )),
        model_override: Arc::new(std::sync::Mutex::new(None)),
        current_purpose: Arc::new(std::sync::Mutex::new("baseline".to_string())),
        caller,
        runtime_info: Arc::new(std::sync::Mutex::new(opencrab_actions::RuntimeInfo {
            default_model: "baseline:model".to_string(),
            active_model: None,
            available_providers: vec!["baseline".to_string()],
            gateway: "baseline".to_string(),
        })),
    }
}

fn seeded_tool_state() -> AppState {
    let state = test_app_state();
    {
        let conn = state.db.lock().expect("lock baseline gateway DB");
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: AGENT_ID.to_string(),
                name: "Baseline Agent".to_string(),
                job_title: Some("Compatibility Probe".to_string()),
                organization: Some("opencrab".to_string()),
                image_url: None,
                persona_name: "Baseline".to_string(),
                personality: Some("deterministic".to_string()),
                instructions: "baseline instructions".to_string(),
                heartbeat_instructions: "baseline heartbeat".to_string(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .expect("seed baseline gateway agent");
        #[cfg(feature = "nostr")]
        opencrab_db::queries::upsert_agent_nostr_config(
            &conn,
            &opencrab_db::queries::AgentNostrConfigRow {
                agent_id: AGENT_ID.to_string(),
                enabled: false,
                relays_json: "[]".to_string(),
                filter_json: "{\"authors\":[],\"keywords\":[],\"kinds\":[]}".to_string(),
                secret_key: "nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqzqujme"
                    .to_string(),
            },
        )
        .expect("seed baseline Nostr config");
    }
    state
}

#[derive(Clone)]
struct LocalMcpServer {
    trusted_name: String,
    tools: Vec<opencrab_mcp::McpTool>,
}

#[async_trait]
impl opencrab_mcp::actions::McpServer for LocalMcpServer {
    fn server_name(&self) -> &str {
        &self.trusted_name
    }

    fn tools(&self) -> &[opencrab_mcp::McpTool] {
        &self.tools
    }

    async fn call_tool(
        &self,
        name: &str,
        args: Value,
    ) -> anyhow::Result<opencrab_mcp::McpToolResult> {
        match args.get("mode").and_then(Value::as_str) {
            Some("tool_error") => Ok(opencrab_mcp::McpToolResult {
                text: "local MCP tool error".to_string(),
                is_error: true,
            }),
            Some("transport_error") => anyhow::bail!("local MCP transport closed"),
            _ => Ok(opencrab_mcp::McpToolResult {
                text: format!(
                    "{name}:{}",
                    args.get("value").and_then(Value::as_str).unwrap_or("ok")
                ),
                is_error: false,
            }),
        }
    }
}

fn mcp_servers() -> Vec<opencrab_mcp::ConnectedServer> {
    let tool = |description: &str| opencrab_mcp::McpTool {
        name: "echo".to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type":"object",
            "properties":{"value":{"type":"string"},"mode":{"type":"string"}},
            "required":["value"]
        }),
    };
    vec![
        opencrab_mcp::ConnectedServer {
            server: Arc::new(LocalMcpServer {
                trusted_name: "public_local".to_string(),
                tools: vec![tool("public local echo")],
            }),
            trusted_only: false,
        },
        opencrab_mcp::ConnectedServer {
            server: Arc::new(LocalMcpServer {
                trusted_name: "trusted_local".to_string(),
                tools: vec![tool("trusted local echo")],
            }),
            trusted_only: true,
        },
    ]
}

fn tool_names(executor: &opencrab_actions::BridgedExecutor) -> Vec<String> {
    // #923: baseline_l2 は全ツール inventory の監査（LLM 投影ではない）。list_tools は depth0 で
    // 常時集合に絞るため、inventory は narrowing 前の層 effective_tool_definitions() で捕捉する。
    let mut names: Vec<_> = executor
        .effective_tool_definitions()
        .into_iter()
        .map(|d| d.definition.name)
        .collect();
    names.sort();
    names
}

pub(super) fn build_executor_with_state(
    caller: opencrab_actions::CallerIdentity,
    depth: u32,
    shell_enabled: bool,
    allowlist: Option<Vec<String>>,
    fixture: &str,
    transport: ToolTransportProfile,
) -> (opencrab_actions::BridgedExecutor, AppState) {
    let context = action_context(caller, fixture);
    let mut state = seeded_tool_state();
    state.db = context.db.clone();
    state.workspace_base = context.workspace.root().to_string_lossy().to_string();
    let fixture_command = shell_enabled.then(|| {
        seed_fixture_executable(&fixture_workspace("process"))
            .expect("seed baseline process fixture")
            .to_string_lossy()
            .to_string()
    });
    #[cfg(feature = "nostr")]
    {
        let connection = state.db.lock().expect("lock baseline Nostr DB");
        opencrab_db::queries::upsert_agent_nostr_config(
            &connection,
            &opencrab_db::queries::AgentNostrConfigRow {
                agent_id: AGENT_ID.to_string(),
                enabled: false,
                relays_json: "[]".to_string(),
                filter_json: "{\"authors\":[],\"keywords\":[],\"kinds\":[]}".to_string(),
                secret_key: "nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqzqujme"
                    .to_string(),
            },
        )
        .expect("seed baseline Nostr config");
    }
    *state.tools_config.write().expect("baseline tools config") = opencrab_actions::ToolsConfig {
        enabled: shell_enabled,
        shell: shell_enabled.then(|| opencrab_actions::ShellToolConfig {
            allowed_commands: vec![fixture_command.expect("enabled shell fixture")],
            allowed_env_vars: Vec::new(),
            ..Default::default()
        }),
    };
    let gateway_actions: Option<Arc<dyn GatewayActions>> = match transport {
        ToolTransportProfile::WithoutTransport => None,
        ToolTransportProfile::Discord => None,
        ToolTransportProfile::Nostr => Some(Arc::new(opencrab_nostr::NostrGatewayActions::new(
            opencrab_nostr::NostaroCli::new(),
        ))),
    };
    let executor = process::build_turn_executor(
        &state,
        process::TurnExecutorWiring {
            context,
            depth,
            gateway_actions,
            subtask_registry: Arc::new(dashmap::DashMap::new()),
            completion_sink: None,
            reply_target: None,
            tool_allowlist: allowlist,
        },
        |caller_is_trusted| {
            Some(Arc::new(opencrab_mcp::McpToolProvider::new(
                mcp_servers(),
                caller_is_trusted,
            )) as Arc<dyn GatewayActions>)
        },
    );
    (executor, state)
}

pub(super) fn build_executor(
    caller: opencrab_actions::CallerIdentity,
    depth: u32,
    shell_enabled: bool,
    allowlist: Option<Vec<String>>,
    fixture: &str,
) -> opencrab_actions::BridgedExecutor {
    build_executor_with_state(
        caller,
        depth,
        shell_enabled,
        allowlist,
        fixture,
        ToolTransportProfile::WithoutTransport,
    )
    .0
}

pub(super) fn subtask_fixture_registry(
    subtask_id: &str,
    sub_session_id: &str,
    parent_session_id: &str,
    steerable: bool,
) -> opencrab_actions::SubtaskRegistry {
    let registry: opencrab_actions::SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    registry.insert(
        subtask_id.to_string(),
        opencrab_actions::SpawnedSubtask {
            abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            session_id: sub_session_id.to_string(),
            parent_session_id: parent_session_id.to_string(),
            agent_id: AGENT_ID.to_string(),
            label: "baseline subtask".to_string(),
            tool_name: "spawn_subtask".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: None,
            caller: opencrab_actions::CallerIdentity::Owner,
            lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            steerable,
        },
    );
    registry
}

pub(super) fn build_subtask_fixture_executor(
    session_id: &str,
    depth: u32,
    registry: opencrab_actions::SubtaskRegistry,
) -> (opencrab_actions::BridgedExecutor, AppState) {
    let mut context = action_context(opencrab_actions::CallerIdentity::Owner, "default");
    context.session_id = Some(session_id.to_string());
    let mut state = seeded_tool_state();
    state.db = context.db.clone();
    state.workspace_base = context.workspace.root().to_string_lossy().to_string();
    let executor = process::build_turn_executor(
        &state,
        process::TurnExecutorWiring {
            context,
            depth,
            gateway_actions: None,
            subtask_registry: registry,
            completion_sink: None,
            reply_target: None,
            tool_allowlist: None,
        },
        |_| None,
    );
    (executor, state)
}

pub(super) fn collect_visibility(catalog: &ToolVisibilityCatalog) -> Result<Value, String> {
    use opencrab_actions::CallerIdentity;
    let mut names = BTreeSet::new();
    let rows = catalog
        .cases
        .iter()
        .map(|scenario| {
            if !names.insert(&scenario.name) {
                return Err(format!(
                    "duplicate tool visibility scenario: {}",
                    scenario.name
                ));
            }
            let caller = match scenario.caller.as_str() {
                "owner" => CallerIdentity::Owner,
                "co_agent" => CallerIdentity::CoAgent {
                    agent_id: "baseline-peer".to_string(),
                },
                "trusted_user" => CallerIdentity::TrustedUser,
                "agent" => CallerIdentity::Agent,
                other => return Err(format!("unknown visibility caller: {other}")),
            };
            let caller_name = match &caller {
                CallerIdentity::Owner => "owner",
                CallerIdentity::Agent => "agent",
                CallerIdentity::TrustedUser => "trusted_user",
                CallerIdentity::CoAgent { .. } => "co_agent",
            };
            let mcp_trusted = !matches!(caller, CallerIdentity::Agent);
            let executor = build_executor_with_state(
                caller,
                scenario.depth,
                scenario.shell_enabled,
                scenario.allowlist.clone(),
                "default",
                scenario.transport,
            )
            .0;
            Ok(json!({
                "name":scenario.name,
                "dimensions": {"caller":caller_name,"depth":scenario.depth,"shell_enabled":scenario.shell_enabled,"transport":scenario.transport.as_str(),"allowlist":scenario.allowlist,"mcp_caller_is_trusted":mcp_trusted},
                "visible_tools": tool_names(&executor)
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({"scenarios":rows}))
}

pub(super) fn required_args(schema: &Value) -> Vec<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn result_json(result: opencrab_core::ActionResult) -> Value {
    let mut value = json!({"success":result.success,"data":result.data,"error":result.error});
    normalize(&mut value);
    value
}
