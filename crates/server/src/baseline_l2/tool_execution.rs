use super::*;

fn has_custom_subtask_observer(fixture: &str) -> bool {
    matches!(
        fixture,
        "subtask_spawn" | "subtask_cancel" | "subtask_steer" | "subtask_report"
    )
}

fn session_log_count(state: &AppState, session_id: &str) -> Result<i64, String> {
    state
        .db
        .lock()
        .map_err(|error| format!("session log observer lock: {error}"))?
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
            rusqlite::params![session_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("session log observer query: {error}"))
}

async fn observe_tool_postcondition(
    executor: &opencrab_actions::BridgedExecutor,
    state: &AppState,
    scenario: &ToolPostcondition,
    name: &str,
) -> Result<Value, String> {
    match (
        &scenario.tool,
        &scenario.method,
        &scenario.uri,
        &scenario.db_query,
    ) {
        (Some(tool), None, None, None) => Ok(result_json(
            executor.execute(tool, &scenario.arguments).await,
        )),
        (None, Some(method), Some(uri), None) => {
            let body = (!scenario.arguments.is_null())
                .then(|| serde_json::to_vec(&scenario.arguments))
                .transpose()
                .map_err(|error| format!("serialize {name} postcondition body: {error}"))?;
            request_once(
                state.clone(),
                name,
                method,
                uri,
                body,
                (!scenario.arguments.is_null()).then_some("application/json"),
            )
            .await
        }
        (None, None, None, Some(query)) if query == "memory_declare_window" => {
            let connection = state
                .db
                .lock()
                .map_err(|error| format!("{name} DB observer lock: {error}"))?;
            let mut value = serde_json::to_value(
                opencrab_db::queries::get_memory_declare_window(&connection, AGENT_ID)
                    .map_err(|error| format!("{name} DB observer: {error}"))?,
            )
            .map_err(|error| format!("{name} DB observer serialize: {error}"))?;
            normalize(&mut value);
            Ok(json!({"db_query":query,"value":value}))
        }
        (None, None, None, Some(query)) if query == "executor_runtime" => {
            let runtime = executor.runtime_state();
            Ok(json!({
                "db_query":query,
                "value":{
                    "model_override":runtime.model_override,
                    "current_purpose":runtime.current_purpose,
                }
            }))
        }
        (None, None, None, Some(query)) if query == "webhook_configs" => {
            let connection = state
                .db
                .lock()
                .map_err(|error| format!("{name} DB observer lock: {error}"))?;
            let rows =
                opencrab_db::queries::list_agent_webhook_config(&connection, Some(AGENT_ID), true)
                    .map_err(|error| format!("{name} DB observer: {error}"))?;
            let mut value = serde_json::to_value(rows)
                .map_err(|error| format!("{name} DB observer serialize: {error}"))?;
            normalize(&mut value);
            Ok(json!({"db_query":query,"value":value}))
        }
        (None, None, None, Some(query)) if query == "inner_voice_logs" => {
            let connection = state
                .db
                .lock()
                .map_err(|error| format!("{name} DB observer lock: {error}"))?;
            let mut statement = connection
                .prepare(
                    "SELECT session_id, log_type, content, speaker_id FROM memory_sessions \
                     WHERE agent_id = ?1 AND log_type = 'inner_voice' ORDER BY id",
                )
                .map_err(|error| format!("{name} DB observer prepare: {error}"))?;
            let rows = statement
                .query_map(rusqlite::params![AGENT_ID], |row| {
                    Ok(json!({
                        "session_id":row.get::<_, String>(0)?,
                        "log_type":row.get::<_, String>(1)?,
                        "content":row.get::<_, String>(2)?,
                        "speaker_id":row.get::<_, Option<String>>(3)?,
                    }))
                })
                .map_err(|error| format!("{name} DB observer query: {error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("{name} DB observer row: {error}"))?;
            Ok(json!({"db_query":query,"rows":rows}))
        }
        (None, None, None, Some(query)) if query == "impressions" => {
            let connection = state
                .db
                .lock()
                .map_err(|error| format!("{name} DB observer lock: {error}"))?;
            let mut value = serde_json::to_value(
                opencrab_db::queries::get_impressions(&connection, AGENT_ID)
                    .map_err(|error| format!("{name} DB observer: {error}"))?,
            )
            .map_err(|error| format!("{name} DB observer serialize: {error}"))?;
            normalize(&mut value);
            Ok(json!({"db_query":query,"value":value}))
        }
        (None, None, None, Some(query)) if query == "reflection_memory" => {
            let connection = state
                .db
                .lock()
                .map_err(|error| format!("{name} DB observer lock: {error}"))?;
            let mut value = serde_json::to_value(
                opencrab_db::queries::get_curated_memories_by_prefix(
                    &connection,
                    AGENT_ID,
                    "reflection",
                )
                .map_err(|error| format!("{name} DB observer: {error}"))?,
            )
            .map_err(|error| format!("{name} DB observer serialize: {error}"))?;
            normalize(&mut value);
            Ok(json!({"db_query":query,"value":value}))
        }
        (None, None, None, Some(query)) if query == "memory_index_state" => {
            let connection = state
                .db
                .lock()
                .map_err(|error| format!("{name} DB observer lock: {error}"))?;
            let mut nodes_statement = connection
                .prepare("SELECT id, title, node_type, source_type, summary, keywords_json FROM memory_index_nodes WHERE agent_id = ?1 ORDER BY id")
                .map_err(|error| format!("{name} nodes observer prepare: {error}"))?;
            let nodes = nodes_statement
                .query_map(rusqlite::params![AGENT_ID], |row| {
                    Ok(json!({"id":row.get::<_,String>(0)?,"title":row.get::<_,String>(1)?,"node_type":row.get::<_,String>(2)?,"source_type":row.get::<_,String>(3)?,"summary":row.get::<_,String>(4)?,"keywords_json":row.get::<_,String>(5)?}))
                })
                .map_err(|error| format!("{name} nodes observer query: {error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("{name} nodes observer row: {error}"))?;
            let mut members_statement = connection
                .prepare("SELECT topic_id, category_id FROM memory_category_members WHERE agent_id = ?1 ORDER BY topic_id, category_id")
                .map_err(|error| format!("{name} members observer prepare: {error}"))?;
            let members = members_statement
                .query_map(rusqlite::params![AGENT_ID], |row| {
                    Ok(json!({"topic_id":row.get::<_,String>(0)?,"category_id":row.get::<_,String>(1)?}))
                })
                .map_err(|error| format!("{name} members observer query: {error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("{name} members observer row: {error}"))?;
            Ok(json!({"db_query":query,"nodes":nodes,"members":members}))
        }
        (None, None, None, Some(query)) if query == "memory_index_config" => {
            let connection = state
                .db
                .lock()
                .map_err(|error| format!("{name} DB observer lock: {error}"))?;
            let mut value = serde_json::to_value(
                opencrab_db::queries::get_memory_index_config(&connection, AGENT_ID)
                    .map_err(|error| format!("{name} DB observer: {error}"))?,
            )
            .map_err(|error| format!("{name} DB observer serialize: {error}"))?;
            normalize(&mut value);
            Ok(json!({"db_query":query,"value":value}))
        }
        _ => Err(format!(
            "postcondition for {name} must select exactly one tool or HTTP method+uri"
        )),
    }
}

pub(super) async fn collect_tool_execution(
    l1: &Value,
    catalog: &ToolScenarioCatalog,
) -> Result<Value, String> {
    use opencrab_actions::CallerIdentity;
    let owner_profiles = [
        (
            ToolTransportProfile::WithoutTransport,
            build_executor(CallerIdentity::Owner, 0, true, None, "default"),
        ),
        (
            ToolTransportProfile::Discord,
            build_executor_with_state(
                CallerIdentity::Owner,
                0,
                true,
                None,
                "default",
                ToolTransportProfile::Discord,
            )
            .0,
        ),
        (
            ToolTransportProfile::Nostr,
            build_executor_with_state(
                CallerIdentity::Owner,
                0,
                true,
                None,
                "default",
                ToolTransportProfile::Nostr,
            )
            .0,
        ),
    ];
    let mut definitions: BTreeMap<String, (usize, FunctionDefinition)> = BTreeMap::new();
    for (profile_index, (_, executor)) in owner_profiles.iter().enumerate() {
        // #923: inventory 監査なので narrowing 前の effective_tool_definitions() で捕捉。
        for definition in executor
            .effective_tool_definitions()
            .into_iter()
            .map(|d| d.definition)
        {
            if let Some((_, existing)) = definitions.get(&definition.name) {
                let mut existing_definition = json!({
                    "description":existing.description,
                    "parameters":existing.parameters,
                });
                let mut observed_definition = json!({
                    "description":definition.description,
                    "parameters":definition.parameters,
                });
                normalize(&mut existing_definition);
                normalize(&mut observed_definition);
                if existing_definition != observed_definition {
                    return Err(format!(
                        "tool {} has inconsistent definitions across transport profiles",
                        definition.name
                    ));
                }
            } else {
                definitions.insert(definition.name.clone(), (profile_index, definition));
            }
        }
    }
    let all_defs: Vec<_> = definitions
        .values()
        .map(|(_, definition)| definition.clone())
        .collect();
    let live_tools: BTreeSet<_> = all_defs
        .iter()
        .map(|definition| definition.name.clone())
        .collect();
    let l1_profiles = l1
        .pointer("/tools/effective_profiles")
        .and_then(Value::as_object)
        .ok_or_else(|| "L1 tool profiles are missing".to_string())?;
    for (profile_name, (profile, executor)) in [
        ("without_transport_surface", &owner_profiles[0]),
        ("discord_turn", &owner_profiles[1]),
        ("nostr_turn", &owner_profiles[2]),
    ] {
        let expected: BTreeSet<_> = l1_profiles
            .get(profile_name)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("L1 tool profile {profile_name} is missing"))?
            .iter()
            .filter_map(|definition| definition["name"].as_str())
            .map(ToOwned::to_owned)
            .collect();
        let observed: BTreeSet<_> = executor
            .effective_tool_definitions()
            .into_iter()
            .map(|definition| definition.definition.name)
            .filter(|name| !name.starts_with("mcp__"))
            .collect();
        if expected != observed {
            return Err(format!(
                "L2 {} profile is not identical to L1 {profile_name}; missing={:?}, unknown={:?}",
                profile.as_str(),
                expected.difference(&observed).collect::<Vec<_>>(),
                observed.difference(&expected).collect::<Vec<_>>()
            ));
        }
    }
    let mut expected_tools: BTreeSet<_> = l1_profiles
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|definition| definition["name"].as_str())
        .map(ToOwned::to_owned)
        .collect();
    expected_tools.extend(
        live_tools
            .iter()
            .filter(|name| name.starts_with("mcp__"))
            .cloned(),
    );
    if live_tools != expected_tools {
        return Err(format!(
            "L2 tool union is not identical to the L1 profile union plus local MCP fixtures; missing={:?}, unknown={:?}",
            expected_tools.difference(&live_tools).collect::<Vec<_>>(),
            live_tools.difference(&expected_tools).collect::<Vec<_>>()
        ));
    }
    let selected_tools: BTreeSet<_> = catalog
        .success_arguments
        .keys()
        .chain(catalog.success_uncollected_l3.keys())
        .cloned()
        .collect();
    let overlap: Vec<_> = catalog
        .success_arguments
        .keys()
        .filter(|name| catalog.success_uncollected_l3.contains_key(*name))
        .cloned()
        .collect();
    if !overlap.is_empty() || live_tools != selected_tools {
        return Err(format!(
            "tool scenario catalog is not a bijection with production tools; overlap={overlap:?}, missing={:?}, unknown={:?}",
            live_tools.difference(&selected_tools).collect::<Vec<_>>(),
            selected_tools.difference(&live_tools).collect::<Vec<_>>()
        ));
    }
    let classified_successes: BTreeSet<_> = catalog
        .effectful_tools
        .union(&catalog.read_only_tools)
        .cloned()
        .collect();
    let classification_overlap: Vec<_> = catalog
        .effectful_tools
        .intersection(&catalog.read_only_tools)
        .collect();
    let successful_scenarios: BTreeSet<_> = catalog.success_arguments.keys().cloned().collect();
    if !classification_overlap.is_empty() || classified_successes != successful_scenarios {
        return Err(format!(
            "successful tool scenarios are not a bijection with read-only/effect-observed classifications; overlap={classification_overlap:?}, missing={:?}, unknown={:?}",
            successful_scenarios.difference(&classified_successes).collect::<Vec<_>>(),
            classified_successes.difference(&successful_scenarios).collect::<Vec<_>>()
        ));
    }
    let missing_postconditions: Vec<_> = catalog
        .effectful_tools
        .iter()
        .filter(|tool| {
            !catalog.postconditions.contains_key(*tool)
                && !catalog
                    .fixtures
                    .get(*tool)
                    .is_some_and(|fixture| has_custom_subtask_observer(fixture))
        })
        .collect();
    if !missing_postconditions.is_empty() {
        return Err(format!(
            "effectful tool scenarios lack postconditions: {missing_postconditions:?}"
        ));
    }
    let read_only_postconditions: Vec<_> = catalog
        .postconditions
        .keys()
        .filter(|tool| !catalog.effectful_tools.contains(*tool))
        .collect();
    if !read_only_postconditions.is_empty() {
        return Err(format!(
            "read-only tool scenarios unexpectedly declare effect postconditions: {read_only_postconditions:?}"
        ));
    }
    let mut required_missing = Vec::new();
    let mut no_required_args_uncollected = Vec::new();
    for def in &all_defs {
        let required = required_args(&def.parameters);
        if required.is_empty() {
            no_required_args_uncollected.push(json!({
                "tool":def.name,
                "facet":"missing_arguments",
                "status":"not_applicable",
                "reason":"observed schema declares no required arguments; invoking {} would not be an argument-missing probe"
            }));
            continue;
        }
        let profile_index = definitions
            .get(&def.name)
            .map(|(profile_index, _)| *profile_index)
            .ok_or_else(|| format!("missing executor profile for {}", def.name))?;
        let result = owner_profiles[profile_index]
            .1
            .execute(&def.name, &json!({}))
            .await;
        required_missing.push(json!({
            "tool":def.name,
            "required_by_observed_schema":required,
            "arguments":{},
            "result":result_json(result)
        }));
    }

    let agent_profiles = [
        build_executor(CallerIdentity::Agent, 0, true, None, "default"),
        build_executor_with_state(
            CallerIdentity::Agent,
            0,
            true,
            None,
            "default",
            ToolTransportProfile::Discord,
        )
        .0,
        build_executor_with_state(
            CallerIdentity::Agent,
            0,
            true,
            None,
            "default",
            ToolTransportProfile::Nostr,
        )
        .0,
    ];
    let mut permission = Vec::new();
    for def in &all_defs {
        let policy = opencrab_actions::tool_policy(&def.name);
        if policy.owner_only || policy.trusted_only || def.name == "mcp__trusted_local__echo" {
            let profile_index = definitions
                .get(&def.name)
                .map(|(profile_index, _)| *profile_index)
                .ok_or_else(|| format!("missing executor profile for {}", def.name))?;
            let result = agent_profiles[profile_index]
                .execute(&def.name, &json!({}))
                .await;
            permission.push(json!({
                "tool":def.name,
                "caller":"agent",
                "policy":{"owner_only":policy.owner_only,"trusted_only":policy.trusted_only,"mcp_trusted_only":def.name == "mcp__trusted_local__echo"},
                "result":result_json(result)
            }));
        }
    }

    let success_cases = &catalog.success_arguments;
    let mut successes = Vec::new();
    let mut unsuccessful_attempts = Vec::new();
    for (tool, arguments) in success_cases {
        if catalog
            .fixtures
            .get(tool)
            .is_some_and(|fixture| has_custom_subtask_observer(fixture))
        {
            continue;
        }
        let fixture = catalog
            .fixtures
            .get(tool)
            .map(String::as_str)
            .unwrap_or("default");
        let (executor, state) = build_executor_with_state(
            CallerIdentity::Owner,
            0,
            true,
            None,
            fixture,
            ToolTransportProfile::WithoutTransport,
        );
        let before = if let Some(postcondition) = catalog.postconditions.get(tool) {
            Some(observe_tool_postcondition(&executor, &state, postcondition, tool).await?)
        } else {
            None
        };
        let mut execution_arguments = arguments.clone();
        if tool == "execute_shell" {
            let command = state
                .tools_config
                .read()
                .map_err(|error| format!("read baseline tools config: {error}"))?
                .shell
                .as_ref()
                .and_then(|shell| shell.allowed_commands.first())
                .cloned()
                .ok_or_else(|| "baseline shell fixture is missing".to_string())?;
            execution_arguments["command"] = command.into();
        }
        let result = executor.execute(tool, &execution_arguments).await;
        let result = result_json(result);
        normalize(&mut execution_arguments);
        if result["success"] == true {
            let postcondition = if let Some(postcondition) = catalog.postconditions.get(tool) {
                let observed =
                    observe_tool_postcondition(&executor, &state, postcondition, tool).await?;
                if postcondition.tool.is_some()
                    && observed["success"] != postcondition.expect_success
                {
                    return Err(format!(
                        "postcondition for {tool} did not satisfy expected success={}: {observed}",
                        postcondition.expect_success
                    ));
                }
                if let Some(expected) = postcondition.expect_status {
                    if observed["status"] != expected {
                        return Err(format!("postcondition for {tool} did not satisfy expected HTTP status={expected}: {observed}"));
                    }
                }
                if before.as_ref() == Some(&observed) {
                    return Err(format!(
                        "postcondition for {tool} did not change; a no-op implementation would pass"
                    ));
                }
                Some(json!({
                    "tool":postcondition.tool,
                    "method":postcondition.method,
                    "uri":postcondition.uri,
                    "db_query":postcondition.db_query,
                    "arguments":postcondition.arguments,
                    "expect_success":postcondition.expect_success,
                    "before":before,
                    "observed":observed
                }))
            } else {
                None
            };
            successes.push(json!({"tool":tool,"arguments":execution_arguments,"result":result,"postcondition":postcondition}));
        } else {
            unsuccessful_attempts.push(json!({
                "tool":tool,
                "arguments":execution_arguments,
                "result":result,
                "status":"uncollected",
                "reason":"the selected local success precondition reached the real implementation but did not return success"
            }));
        }
    }

    for (tool, fixture) in &catalog.fixtures {
        let Some((session_id, depth, steerable)) = (match fixture.as_str() {
            "subtask_spawn" => Some((TOOL_SESSION_ID, 0, false)),
            "subtask_cancel" => Some((TOOL_SESSION_ID, 0, false)),
            "subtask_steer" => Some((TOOL_SESSION_ID, 0, true)),
            "subtask_report" => Some(("subtask-baseline-subtask", 1, true)),
            _ => None,
        }) else {
            continue;
        };
        let arguments = catalog
            .success_arguments
            .get(tool)
            .ok_or_else(|| format!("fixture {fixture} has no success arguments for {tool}"))?;
        let registry = if fixture == "subtask_spawn" {
            Arc::new(dashmap::DashMap::new())
        } else {
            subtask_fixture_registry(
                "baseline-subtask",
                "subtask-baseline-subtask",
                TOOL_SESSION_ID,
                steerable,
            )
        };
        let registry_observer = registry.clone();
        let (executor, state) = build_subtask_fixture_executor(session_id, depth, registry);
        let before_logs = if matches!(fixture.as_str(), "subtask_steer" | "subtask_report") {
            let observed_session = if fixture == "subtask_steer" {
                "subtask-baseline-subtask"
            } else {
                TOOL_SESSION_ID
            };
            Some(session_log_count(&state, observed_session)?)
        } else {
            None
        };
        let raw_result = executor.execute(tool, arguments).await;
        let raw_data = raw_result.data.clone();
        let result = result_json(raw_result);
        let effect = match fixture.as_str() {
            "subtask_spawn" if result["success"] == true => {
                let subtask_id = raw_data["subtask_id"]
                    .as_str()
                    .ok_or_else(|| "spawn_subtask succeeded without a subtask_id".to_string())?;
                let session_id = raw_data["session_id"]
                    .as_str()
                    .ok_or_else(|| "spawn_subtask succeeded without a session_id".to_string())?;
                let durable_session_registered = state
                    .db
                    .lock()
                    .map_err(|error| format!("spawn_subtask DB observer lock: {error}"))?
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                        rusqlite::params![session_id],
                        |row| row.get::<_, bool>(0),
                    )
                    .map_err(|error| format!("spawn_subtask DB observer: {error}"))?;
                if !durable_session_registered {
                    return Err(
                        "spawn_subtask returned success without registering its durable session"
                            .to_string(),
                    );
                }
                Some(json!({
                    "accepted":true,
                    "registry_registered_when_observed":registry_observer.contains_key(subtask_id),
                    "durable_session_registered":true,
                    "completion":{
                        "status":"uncollected",
                        "reason":"the production spawn path was accepted and registered; its background LLM completion has no provider and is not claimed as success"
                    }
                }))
            }
            "subtask_cancel" if result["success"] == true => {
                if registry_observer.contains_key("baseline-subtask") {
                    return Err(
                        "cancel_subtask returned success without removing the registry entry"
                            .to_string(),
                    );
                }
                Some(json!({"registry_before":true,"registry_after":false}))
            }
            "subtask_steer" | "subtask_report" if result["success"] == true => {
                let observed_session = if fixture == "subtask_steer" {
                    "subtask-baseline-subtask"
                } else {
                    TOOL_SESSION_ID
                };
                let after_logs = session_log_count(&state, observed_session)?;
                if before_logs == Some(after_logs) {
                    return Err(format!(
                        "{tool} returned success without recording its delivered effect"
                    ));
                }
                Some(
                    json!({"session_id":observed_session,"logs_before":before_logs,"logs_after":after_logs}),
                )
            }
            _ => None,
        };
        if result["success"] == true {
            successes
                .push(json!({"tool":tool,"arguments":arguments,"result":result,"effect":effect}));
        } else {
            unsuccessful_attempts.push(json!({
                "tool":tool,
                "arguments":arguments,
                "result":result,
                "status":"uncollected",
                "reason":"the selected in-memory running-subtask precondition reached the real implementation but did not return success"
            }));
        }
    }

    let mut forwarding = Vec::new();
    for (name, scenario) in &catalog.forwarding {
        if !live_tools.contains(&scenario.tool) {
            return Err(format!(
                "forwarding scenario {name} selects unknown tool {}",
                scenario.tool
            ));
        }
        let result = owner_profiles[0]
            .1
            .execute(&scenario.tool, &scenario.arguments)
            .await;
        forwarding.push(json!({"name":name,"tool":scenario.tool,"arguments":scenario.arguments,"result":result_json(result)}));
    }

    let success_names: BTreeSet<_> = successes
        .iter()
        .filter_map(|v| v["tool"].as_str())
        .collect();
    let unobserved_effectful_successes: Vec<_> = successes
        .iter()
        .filter(|row| {
            row["tool"].as_str().is_some_and(|tool| {
                catalog.effectful_tools.contains(tool)
                    && row["postcondition"].is_null()
                    && row["effect"].is_null()
            })
        })
        .filter_map(|row| row["tool"].as_str())
        .collect();
    if !unobserved_effectful_successes.is_empty() {
        return Err(format!(
            "effectful successful tools lack a nonempty observation: {unobserved_effectful_successes:?}"
        ));
    }
    let success_uncollected: Vec<_> = all_defs
        .iter()
        .filter(|d| !success_names.contains(d.name.as_str()))
        .map(|d| {
            let reason = catalog
                .success_uncollected_l3
                .get(&d.name)
                .map(String::as_str)
                .unwrap_or("selected success scenario did not satisfy its postcondition");
            json!({
                "tool":d.name,
                "facet":"success",
                "status":"uncollected",
                "reason":reason
            })
        })
        .collect();

    if !unsuccessful_attempts.is_empty() {
        return Err(format!(
            "selected tool success scenario did not succeed: {}",
            serde_json::to_string(&unsuccessful_attempts)
                .unwrap_or_else(|_| "<unserializable attempts>".to_string())
        ));
    }

    let uncollected = [no_required_args_uncollected, success_uncollected].concat();
    Ok(json!({
        "missing_arguments":required_missing,
        "permission_denied":permission,
        "successful_calls":successes,
        "unsuccessful_attempts":unsuccessful_attempts,
        "forwarding_failures":forwarding,
        "uncollected":uncollected
    }))
}
