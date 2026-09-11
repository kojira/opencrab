use super::*;

fn seeded_state() -> Result<AppState, String> {
    let mut state = test_app_state();
    let workspace = fixture_workspace("workspace");
    fs::create_dir_all(&workspace)
        .map_err(|error| format!("create baseline workspace: {error}"))?;
    fs::write(workspace.join("baseline.txt"), b"baseline\n")
        .map_err(|error| format!("seed baseline workspace: {error}"))?;
    let executable = seed_fixture_executable(&fixture_workspace("process"))?;
    let mut llm_config: crate::config::LlmConfig =
        toml::from_str("[providers.codex]\n[providers.cursor]\n")
            .map_err(|error| format!("build deterministic diagnostic config: {error}"))?;
    for provider in ["codex", "cursor"] {
        llm_config
            .providers
            .get_mut(provider)
            .ok_or_else(|| format!("deterministic diagnostic config omitted {provider}"))?
            .binary_path = executable.to_string_lossy().to_string();
    }
    state.llm_config = Arc::new(llm_config);
    state.workspace_base = workspace.to_string_lossy().to_string();
    state.intake = Arc::new(crate::config::IntakeConfig {
        secrets: [("baseline-source".to_string(), "baseline-secret".to_string())]
            .into_iter()
            .collect(),
        routes: vec![crate::config::IntakeRoute {
            source: "baseline-source".to_string(),
            event_type: "baseline.event".to_string(),
            agent_id: AGENT_ID.to_string(),
        }],
        ..Default::default()
    });

    let conn = state.db.lock().map_err(|e| format!("lock DB: {e}"))?;
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
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .map_err(|e| format!("seed agent: {e}"))?;
    opencrab_db::queries::insert_session(
        &conn,
        &opencrab_db::queries::SessionRow {
            id: SESSION_ID.to_string(),
            mode: "baseline".to_string(),
            theme: "Compatibility".to_string(),
            phase: "active".to_string(),
            turn_number: 0,
            status: "active".to_string(),
            participant_ids_json: format!(r#"["{AGENT_ID}"]"#),
            facilitator_id: None,
            done_count: 0,
            max_turns: Some(1),
            metadata_json: None,
        },
    )
    .map_err(|e| format!("seed session: {e}"))?;
    opencrab_db::queries::insert_skill(
        &conn,
        &opencrab_db::queries::SkillRow {
            id: "baseline-skill".to_string(),
            agent_id: AGENT_ID.to_string(),
            name: "Seed Skill".to_string(),
            description: "seed".to_string(),
            situation_pattern: "seed".to_string(),
            guidance: "seed".to_string(),
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
    .map_err(|e| format!("seed skill: {e}"))?;
    opencrab_db::queries::insert_skill(
        &conn,
        &opencrab_db::queries::SkillRow {
            id: "baseline-archived-skill".to_string(),
            agent_id: AGENT_ID.to_string(),
            name: "Archived Seed Skill".to_string(),
            description: "archived seed".to_string(),
            situation_pattern: "archived seed".to_string(),
            guidance: "archived seed".to_string(),
            source_type: "baseline".to_string(),
            source_context: None,
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
            permission: "private".to_string(),
            archived: true,
            created_caller: Some("owner".to_string()),
            agent_visible: false,
        },
    )
    .map_err(|e| format!("seed archived skill: {e}"))?;
    opencrab_db::queries::insert_soul_preset(
        &conn,
        &opencrab_db::queries::SoulPresetRow {
            id: "baseline-preset".to_string(),
            agent_id: AGENT_ID.to_string(),
            preset_name: "Seed Preset".to_string(),
            persona_name: "Seed Persona".to_string(),
            custom_traits_json: Some("{}".to_string()),
        },
    )
    .map_err(|e| format!("seed soul preset: {e}"))?;
    opencrab_db::queries::upsert_curated_memory(
        &conn,
        &opencrab_db::queries::CuratedMemoryRow {
            id: "baseline-memory".to_string(),
            agent_id: AGENT_ID.to_string(),
            category: "baseline".to_string(),
            content: "seed memory".to_string(),
            created_at: "ignored by upsert".to_string(),
        },
    )
    .map_err(|e| format!("seed curated memory: {e}"))?;
    opencrab_db::queries::insert_trusted_co_agent(
        &conn,
        &opencrab_db::queries::TrustedCoAgentRow {
            id: "baseline-co-agent-row".to_string(),
            agent_id: AGENT_ID.to_string(),
            co_agent_id: "baseline-peer".to_string(),
            allowed_actions: None,
            created_by: "owner".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        },
    )
    .map_err(|e| format!("seed co-agent: {e}"))?;
    opencrab_db::queries::add_trusted_user(
        &conn,
        "web",
        "baseline-trusted-row",
        AGENT_ID,
        "baseline-user",
        opencrab_db::queries::TrustedUserPermission::User,
        "owner",
        "2026-01-01T00:00:00Z",
        "Baseline User",
    )
    .map_err(|e| format!("seed trusted user: {e}"))?;
    opencrab_db::queries::upsert_channel_config(
        &conn,
        &opencrab_db::queries::ChannelConfigRow {
            channel_id: "baseline-channel".to_string(),
            agent_id: AGENT_ID.to_string(),
            guild_id: "baseline-guild".to_string(),
            channel_name: "baseline".to_string(),
            readable: true,
            writable: true,
            whitelisted: true,
            heartbeat_enabled: false,
            heartbeat_interval_secs: None,
            heartbeat_instructions: String::new(),
        },
    )
    .map_err(|e| format!("seed channel config: {e}"))?;
    opencrab_db::queries::add_agent_allowed_command(
        &conn,
        AGENT_ID,
        "baseline-command",
        "baseline",
    )
    .map_err(|e| format!("seed allowed command: {e}"))?;
    opencrab_db::queries::upsert_agent_discord_config(
        &conn,
        &opencrab_db::queries::AgentDiscordConfigRow {
            agent_id: AGENT_ID.to_string(),
            bot_token: "baseline-not-a-credential".to_string(),
            owner_discord_id: "baseline-owner".to_string(),
            enabled: true,
        },
    )
    .map_err(|e| format!("seed Discord config: {e}"))?;
    opencrab_db::queries::upsert_agent_mcp_server(
        &conn,
        &opencrab_db::queries::AgentMcpServerRow {
            agent_id: AGENT_ID.to_string(),
            name: "baseline".to_string(),
            command: "false".to_string(),
            args_json: "[]".to_string(),
            env_json: "{}".to_string(),
            trusted_only: false,
            enabled: false,
        },
    )
    .map_err(|e| format!("seed MCP config: {e}"))?;
    opencrab_db::queries::upsert_agent_nostr_config(
        &conn,
        &opencrab_db::queries::AgentNostrConfigRow {
            agent_id: AGENT_ID.to_string(),
            secret_key: "nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqzqujme"
                .to_string(),
            relays_json: "[]".to_string(),
            filter_json: "{\"authors\":[],\"keywords\":[],\"kinds\":[]}".to_string(),
            enabled: true,
        },
    )
    .map_err(|e| format!("seed Nostr config: {e}"))?;
    opencrab_db::queries::set_voice_config_override(
        &conn,
        r#"{"enabled":false,"stt":{"language":"ja"}}"#,
    )
    .map_err(|e| format!("seed voice config: {e}"))?;
    opencrab_db::queries::insert_index_node(
        &conn,
        &opencrab_db::queries::IndexNodeRow {
            id: "baseline-http-index-node".to_string(),
            agent_id: AGENT_ID.to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: "session_log".to_string(),
            title: "Baseline HTTP Topic".to_string(),
            summary: "baseline HTTP index seed".to_string(),
            start_log_id: None,
            end_log_id: None,
            source_session_id: None,
            date_from: Some("2026-01-01".to_string()),
            date_to: Some("2026-01-01".to_string()),
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: Some("t-http".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .map_err(|e| format!("seed HTTP memory index: {e}"))?;
    opencrab_db::queries::insert_agent_schedule(
        &conn,
        &opencrab_db::queries::AgentScheduleRow {
            id: None,
            agent_id: AGENT_ID.to_string(),
            session_id: format!("nostr-{AGENT_ID}"),
            cron_expr: "0 0 * * *".to_string(),
            timezone: "Asia/Tokyo".to_string(),
            message: "seed schedule".to_string(),
            enabled: false,
            anchor_at: None,
            last_fired_at: None,
        },
    )
    .map_err(|e| format!("seed schedule: {e}"))?;
    drop(conn);
    Ok(state)
}

fn scenario_key(method: &str, path: &str) -> String {
    format!("{method} {path}")
}

fn concrete_uri(
    catalog: &HttpScenarioCatalog,
    template: &str,
    missing: bool,
) -> Result<String, String> {
    let mut path = if let Some(value) = catalog.path_overrides.get(template) {
        if missing {
            value.missing.clone()
        } else {
            value.normal.clone()
        }
    } else {
        template.to_string()
    };
    for (placeholder, value) in &catalog.path_parameters {
        path = path.replace(
            &format!("{{{placeholder}}}"),
            if missing {
                &value.missing
            } else {
                &value.normal
            },
        );
    }
    if path.contains('{') {
        return Err(format!(
            "scenario catalog has no value for path template {template}"
        ));
    }
    if !missing {
        if let Some(suffix) = catalog.query_suffixes.get(template) {
            path.push_str(suffix);
        }
    }
    Ok(path)
}

pub(super) async fn request_once(
    state: AppState,
    name: &str,
    method: &str,
    uri: &str,
    body: Option<Vec<u8>>,
    content_type: Option<&str>,
) -> Result<Value, String> {
    let app = create_router(state);
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    if uri.starts_with("/api/hooks/baseline-source") {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let raw = body.as_deref().unwrap_or_default();
        let mut mac = Hmac::<Sha256>::new_from_slice(b"baseline-secret")
            .map_err(|e| format!("{name}: HMAC setup: {e}"))?;
        mac.update(raw);
        let signature = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        builder = builder.header("x-hook-signature", signature);
    }
    let request = builder
        .body(body.map(Body::from).unwrap_or_else(Body::empty))
        .map_err(|e| format!("{name}: build request: {e}"))?;
    let response = app
        .oneshot(request)
        .await
        .map_err(|e| format!("{name}: router call: {e}"))?;
    let status = response.status().as_u16();
    let mut headers: Vec<Value> = response
        .headers()
        .iter()
        .filter(|(name, _)| name.as_str() != "content-length")
        .map(|(name, value)| {
            value
                .to_str()
                .map(|v| json!({"name":name.as_str(),"value":v}))
                .map_err(|e| format!("{name}: invalid response header: {e}"))
        })
        .collect::<Result<_, _>>()?;
    headers.sort_by_key(|a| a.to_string());
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .map_err(|e| format!("{name}: read response: {e}"))?;
    let mut captured_body = serde_json::from_slice::<Value>(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    normalize(&mut captured_body);
    Ok(json!({
        "status": status,
        "headers": headers,
        "body": captured_body,
    }))
}

async fn observe_http_postcondition(
    state: AppState,
    catalog: &HttpScenarioCatalog,
    observer: &HttpPostcondition,
    name: &str,
) -> Result<Value, String> {
    match (&observer.method, &observer.path, &observer.db_query) {
        (Some(method), Some(path), None) => {
            let uri = concrete_uri(catalog, path, false)?;
            let body = observer
                .body
                .as_ref()
                .map(serde_json::to_vec)
                .transpose()
                .map_err(|error| format!("serialize {name} postcondition body: {error}"))?;
            request_once(
                state,
                name,
                method,
                &uri,
                body,
                observer.body.as_ref().map(|_| "application/json"),
            )
            .await
        }
        (None, None, Some(query)) if query == "agent_inbox" => {
            let connection = state
                .db
                .lock()
                .map_err(|error| format!("{name} DB observer lock: {error}"))?;
            let mut statement = connection
                .prepare(
                    "SELECT agent_id, source, event_type, dedup_key, payload_json, \
                     processed_at IS NOT NULL FROM agent_inbox ORDER BY source, dedup_key",
                )
                .map_err(|error| format!("{name} DB observer prepare: {error}"))?;
            let rows = statement
                .query_map([], |row| {
                    Ok(json!({
                        "agent_id":row.get::<_, String>(0)?,
                        "source":row.get::<_, String>(1)?,
                        "event_type":row.get::<_, String>(2)?,
                        "dedup_key":row.get::<_, String>(3)?,
                        "payload_json":row.get::<_, String>(4)?,
                        "processed":row.get::<_, bool>(5)?,
                    }))
                })
                .map_err(|error| format!("{name} DB observer query: {error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("{name} DB observer row: {error}"))?;
            Ok(json!({"db_query":query,"rows":rows}))
        }
        _ => Err(format!(
            "HTTP postcondition for {name} must select exactly one method+path or db_query"
        )),
    }
}

pub(super) async fn collect_http(
    l1: &Value,
    catalog: &HttpScenarioCatalog,
) -> Result<Value, String> {
    let routes = l1
        .pointer("/http/routes")
        .and_then(Value::as_array)
        .ok_or_else(|| "L1 /http/routes is missing or not an array".to_string())?;
    let mut live_keys = BTreeSet::new();
    for route in routes {
        let path = route["path"]
            .as_str()
            .ok_or_else(|| "L1 route has no string path".to_string())?;
        for method in route["methods"]
            .as_array()
            .ok_or_else(|| format!("L1 route {path} has no methods"))?
        {
            live_keys.insert(scenario_key(
                method
                    .as_str()
                    .ok_or_else(|| format!("L1 route {path} has a non-string method"))?,
                path,
            ));
        }
    }
    for key in catalog
        .normal_bodies
        .keys()
        .chain(catalog.normal_uncollected_l3.keys())
        .chain(catalog.bodyless_alternates.keys())
        .chain(catalog.mutation_postconditions.keys())
        .chain(catalog.successful_non_mutations.keys())
    {
        if !live_keys.contains(key) {
            return Err(format!(
                "scenario catalog contains non-production route: {key}"
            ));
        }
    }

    let mut probes = Vec::new();
    let mut uncollected = Vec::new();
    for route in routes {
        let path = route["path"]
            .as_str()
            .ok_or_else(|| "L1 route has no string path".to_string())?;
        let methods = route["methods"]
            .as_array()
            .ok_or_else(|| format!("L1 route {path} has no methods"))?;
        for method in methods {
            let method = method
                .as_str()
                .ok_or_else(|| format!("L1 route {path} has a non-string method"))?;
            let key = scenario_key(method, path);
            let stem = format!(
                "{}__{}",
                method.to_ascii_lowercase(),
                path.replace('/', "_").replace(['{', '}', '*'], "")
            );

            if let Some(reason) = catalog.normal_uncollected_l3.get(&key) {
                uncollected.push(json!({
                    "name":format!("{stem}__normal"), "method":method, "path":path,
                    "branch":"normal", "status":"uncollected", "reason":reason, "level":"L3"
                }));
            } else {
                let catalog_uri = concrete_uri(catalog, path, false)?;
                let body_value = catalog.normal_bodies.get(&key).cloned();
                if matches!(method, "POST" | "PUT" | "PATCH")
                    && body_value.is_none()
                    && !catalog.bodyless_alternates.contains_key(&key)
                {
                    return Err(format!(
                        "mutating route has neither body nor explicit bodyless classification: {key}"
                    ));
                }
                let body = body_value
                    .as_ref()
                    .map(serde_json::to_vec)
                    .transpose()
                    .map_err(|error| format!("serialize {stem} normal body: {error}"))?;
                let state = seeded_state()?;
                let uri = materialize_uri(&catalog_uri)?;
                let artifact_uri = captured_uri(&catalog_uri);
                let observer = catalog.mutation_postconditions.get(&key);
                let before = if let Some(observer) = observer {
                    Some(
                        observe_http_postcondition(
                            state.clone(),
                            catalog,
                            observer,
                            &format!("{stem}__effect_before"),
                        )
                        .await?,
                    )
                } else {
                    None
                };
                let response = request_once(
                    state.clone(),
                    &format!("{stem}__normal"),
                    method,
                    &uri,
                    body,
                    body_value.as_ref().map(|_| "application/json"),
                )
                .await?;
                let successful_mutation = matches!(method, "POST" | "PUT" | "PATCH" | "DELETE")
                    && response["status"]
                        .as_u64()
                        .is_some_and(|status| (200..300).contains(&status));
                let effect = if successful_mutation {
                    if let Some(observer) = observer {
                        let after = observe_http_postcondition(
                            state,
                            catalog,
                            observer,
                            &format!("{stem}__effect_after"),
                        )
                        .await?;
                        if before.as_ref() == Some(&after) {
                            return Err(format!(
                                "HTTP postcondition for {key} did not change; a no-op handler would pass"
                            ));
                        }
                        json!({"status":"observed","observer":{"method":observer.method,"path":observer.path,"body":observer.body,"db_query":observer.db_query},"before":before,"after":after})
                    } else if let Some(reason) = catalog.successful_non_mutations.get(&key) {
                        json!({"status":"not_applicable","reason":reason})
                    } else {
                        return Err(format!(
                            "successful HTTP mutation lacks an independent read-back or explicit read-only classification: {key}"
                        ));
                    }
                } else {
                    Value::Null
                };
                probes.push(json!({
                    "name":format!("{stem}__normal"),
                    "selection":"normal_or_local_precondition_branch",
                    "request":{"method":method,"uri":artifact_uri,"body":body_value},
                    "response":response,
                    "effect":effect
                }));
            }

            let (uri, body, content_type, selection) = if matches!(method, "POST" | "PUT" | "PATCH")
            {
                if let Some(alternate) = catalog.bodyless_alternates.get(&key) {
                    match alternate {
                        AlternateScenario::MissingResource => (
                            concrete_uri(catalog, path, true)?,
                            None,
                            None,
                            "missing_resource_rejection",
                        ),
                        AlternateScenario::NotApplicable { reason } => {
                            uncollected.push(json!({"name":format!("{stem}__reject_or_absent"),"method":method,"path":path,"branch":"input_rejection","status":"not_applicable","reason":reason}));
                            continue;
                        }
                        AlternateScenario::Uncollected { reason } => {
                            uncollected.push(json!({"name":format!("{stem}__reject_or_absent"),"method":method,"path":path,"branch":"input_rejection","status":"uncollected","reason":reason}));
                            continue;
                        }
                    }
                } else if catalog.normal_bodies.contains_key(&key) {
                    (
                        concrete_uri(catalog, path, false)?,
                        Some(b"{".to_vec()),
                        Some("application/json"),
                        "malformed_json_rejection",
                    )
                } else {
                    return Err(format!(
                        "bodyless route has no alternate classification: {key}"
                    ));
                }
            } else {
                (
                    concrete_uri(catalog, path, true)?,
                    None,
                    None,
                    "resource_absence_or_empty_state",
                )
            };
            let response = request_once(
                seeded_state()?,
                &format!("{stem}__reject_or_absent"),
                method,
                &uri,
                body,
                content_type,
            )
            .await?;
            let status = response["status"].as_u64().unwrap_or_default();
            if matches!(
                selection,
                "malformed_json_rejection" | "missing_resource_rejection"
            ) && !(400..500).contains(&status)
            {
                return Err(format!(
                    "{key} {selection} did not reject: observed HTTP {status}"
                ));
            }
            probes.push(json!({
                "name":format!("{stem}__reject_or_absent"), "selection":selection,
                "request":{"method":method,"uri":uri,"body_utf8":if content_type.is_some(){Value::String("{".to_string())}else{Value::Null}},
                "response":response
            }));
        }
    }
    Ok(json!({"source_route_count":routes.len(),"probes":probes,"uncollected":uncollected}))
}
