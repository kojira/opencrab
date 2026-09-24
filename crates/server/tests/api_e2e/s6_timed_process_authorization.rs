struct S6TimedProcessProbe {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl opencrab_gateway::GatewayActions for S6TimedProcessProbe {
    fn definitions(&self) -> Vec<opencrab_gateway::GatewayActionDef> {
        vec![opencrab_gateway::GatewayActionDef {
            name: "s6_timed_process_probe".to_string(),
            description: "S6 timed/subtask production-seam probe".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            class: opencrab_gateway::ToolClass {
                dispatch: opencrab_gateway::DispatchMode::Inline,
                sub_engine: opencrab_gateway::SubEngineAccess::Allowed,
                sharing: opencrab_gateway::ToolSharing::AgentBound,
            },
        }]
    }

    async fn execute(
        &self,
        _name: &str,
        _args: &serde_json::Value,
        _ctx: &opencrab_gateway::GatewayCallContext,
    ) -> opencrab_gateway::GatewayActionResult {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        opencrab_gateway::GatewayActionResult {
            success: true,
            data: Some(serde_json::json!({"ok": true})),
            error: None,
        }
    }
}

#[tokio::test]
async fn s6_actual_process_depth_one_revalidates_before_model_and_tool_effects() {
    for mutation in ["revoke", "revision_bump"] {
        let (app, db, mock, state) = create_test_app_with_state();
        let (agent_id, _app) =
            create_test_agent_named(app, "S6TimedProcess", "TestPersona").await;
        {
            let conn = db.lock().unwrap();
            opencrab_db::queries::insert_trusted_co_agent(
                &conn,
                &opencrab_db::queries::TrustedCoAgentRow {
                    id: format!("relationship-{mutation}"),
                    agent_id: agent_id.clone(),
                    co_agent_id: "peer-agent".to_string(),
                    allowed_actions: None,
                    created_by: "owner".to_string(),
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    relationship_revision: 1,
                    active: true,
                },
            )
            .unwrap();
        }

        mock.push_tool_call_response(vec![ToolCall {
            id: format!("tc-{mutation}"),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "s6_timed_process_probe".to_string(),
                arguments: "{}".to_string(),
            },
        }]);
        mock.push_text_response("done\nNO_REPLY");
        let probe = Arc::new(S6TimedProcessProbe {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let req = opencrab_actions::RunRequest::new(
            &agent_id,
            "S6TimedProcess",
            format!("subtask-{mutation}"),
            "system",
            "user: run the probe",
            "extgate",
            opencrab_actions::CallerIdentity::CoAgent {
                agent_id: "peer-agent".to_string(),
            },
        )
        .with_relationship_authority(
            opencrab_core::authorization::RelationshipAuthority {
                co_agent_id: "peer-agent".to_string(),
                relationship_revision: 1,
            },
        )
        .with_gateway_actions(probe.clone())
        .with_depth(1);

        // Change the real persisted authority after the depth-1 work is fully prepared and
        // immediately before it enters the process boundary.
        {
            let conn = db.lock().unwrap();
            match mutation {
                "revoke" => assert!(opencrab_db::queries::delete_trusted_co_agent(
                    &conn,
                    &agent_id,
                    "peer-agent",
                )
                .unwrap()),
                "revision_bump" => {
                    assert!(opencrab_db::queries::bump_trusted_co_agent_revision(
                        &conn,
                        &agent_id,
                        "peer-agent",
                    )
                    .unwrap())
                }
                _ => unreachable!(),
            }
        }
        let result = opencrab_server::process::run_agent_response(&state, req).await;
        let model_calls = mock.system_prompts().len();
        let tool_calls = probe.calls.load(std::sync::atomic::Ordering::SeqCst);
        let rejected_at_timed_boundary = result.as_ref().err().is_some_and(|error| {
            error
                .to_string()
                .contains("authorization_revoked:timed_subtask_continuation")
        });
        assert!(
            rejected_at_timed_boundary && model_calls == 0 && tool_calls == 0,
            "{mutation}: result={result:?}, model_calls={model_calls}, tool_calls={tool_calls}"
        );
    }
}
