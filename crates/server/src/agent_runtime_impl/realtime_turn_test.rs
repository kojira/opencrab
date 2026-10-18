#![cfg(test)]
//! D-1085 P1: a realtime-profile session turn is a lightweight single generation; a
//! default-profile turn is unchanged.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opencrab_actions::{run_session_turn, AgentRuntime, CallerIdentity, RunRequest, TurnModelProfile};
use opencrab_llm::message::{ChatRequest, ChatResponse, Choice, FinishReason, Message};
use opencrab_llm::traits::{LlmProvider, ModelInfo};

type Requests = Arc<Mutex<Vec<ChatRequest>>>;

/// Provider `pa` with model `small`; records every request and answers with `reply`.
struct CapturingProvider {
    requests: Requests,
    reply: Arc<Mutex<String>>,
}

#[async_trait]
impl LlmProvider for CapturingProvider {
    fn name(&self) -> &str {
        "pa"
    }

    async fn available_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
        Ok(vec![ModelInfo {
            id: "small".to_string(),
            name: "small".to_string(),
            context_window: 0,
            supports_function_calling: true,
            supports_vision: false,
        }])
    }

    async fn chat_completion(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.requests.lock().unwrap().push(request.clone());
        let reply = self.reply.lock().unwrap().clone();
        Ok(ChatResponse {
            id: "r".to_string(),
            model: request.model,
            choices: vec![Choice {
                index: 0,
                message: Message::assistant(reply),
                finish_reason: Some(FinishReason::Stop),
            }],
            usage: Default::default(),
            created: 0,
        })
    }
}

struct Fixture {
    state: crate::AppState,
    requests: Requests,
    reply: Arc<Mutex<String>>,
}

fn log_row(
    log_type: &str,
    speaker: Option<&str>,
    content: &str,
    metadata: Option<&str>,
) -> opencrab_db::queries::SessionLogRow {
    opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: "a1".to_string(),
        session_id: "sess-a".to_string(),
        log_type: log_type.to_string(),
        content: content.to_string(),
        speaker_id: speaker.map(str::to_string),
        turn_number: None,
        metadata_json: metadata.map(str::to_string),
        created_at: None,
    }
}

/// Agent `a1` with SOUL, instructions, curated memory, a visible skill, effort `high` and web
/// search on; session `sess-a` holds 40 speech rows interleaved with tool and system rows.
fn fixture() -> Fixture {
    let state = crate::test_app_state();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_model_pricing(
            &conn,
            &opencrab_db::queries::ModelPricingRow {
                provider: "pa".to_string(),
                model: "small".to_string(),
                input_price_per_1m: 0.0,
                output_price_per_1m: 0.0,
                context_window: Some(200_000),
                max_output_tokens: Some(4_000),
                cached_input_price_per_1m: None,
                cache_write_price_per_1m: None,
            },
        )
        .unwrap();
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: "a1".to_string(),
                name: "Kani".to_string(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "kani-persona".to_string(),
                personality: Some("SOUL: cheerful and brief.".to_string()),
                instructions: "INSTRUCTIONS-BODY".to_string(),
                model: Some("pa:small".to_string()),
                reasoning_effort: Some("high".to_string()),
                web_search: Some(true),
                metadata_json: None,
            },
        )
        .unwrap();
        opencrab_db::queries::upsert_curated_memory(
            &conn,
            &opencrab_db::queries::CuratedMemoryRow {
                id: "m1".to_string(),
                agent_id: "a1".to_string(),
                category: "long_term".to_string(),
                content: "LONG-TERM-BODY".to_string(),
                created_at: String::new(),
            },
        )
        .unwrap();
        opencrab_db::queries::insert_skill(
            &conn,
            &opencrab_db::queries::SkillRow {
                id: "s1".to_string(),
                agent_id: "a1".to_string(),
                name: "visible-skill".to_string(),
                description: "SKILL-DESCRIPTION".to_string(),
                situation_pattern: String::new(),
                guidance: String::new(),
                source_type: "manual".to_string(),
                source_context: None,
                file_path: None,
                effectiveness: None,
                usage_count: 0,
                is_active: true,
                permission: "private".to_string(),
                archived: false,
                created_caller: None,
                agent_visible: true,
            },
        )
        .unwrap();
        opencrab_db::queries::insert_session(
            &conn,
            &opencrab_db::queries::SessionRow {
                id: "sess-a".to_string(),
                mode: "generic".to_string(),
                theme: "sess-a".to_string(),
                phase: "active".to_string(),
                turn_number: 0,
                status: "active".to_string(),
                participant_ids_json: serde_json::json!(["a1"]).to_string(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: None,
            },
        )
        .unwrap();
        for i in 0..40 {
            let at = format!("2026-10-18T10:{:02}:00+00:00", i);
            let (speaker, content) = if i % 2 == 0 {
                ("u1", format!("user-speech-{i:02}"))
            } else {
                ("a1", format!("agent-speech-{i:02}"))
            };
            let owner = (i % 2 == 0).then_some(r#"{"owner":true}"#);
            opencrab_db::queries::insert_session_log_at(
                &conn,
                &log_row("speech", Some(speaker), &content, owner),
                &at,
            )
            .unwrap();
            if i % 10 == 5 {
                opencrab_db::queries::insert_session_log_at(
                    &conn,
                    &log_row("tool_call", Some("a1"), &format!("tool-call-{i:02}"), None),
                    &at,
                )
                .unwrap();
                opencrab_db::queries::insert_session_log_at(
                    &conn,
                    &log_row("tool_result", Some("a1"), &format!("tool-result-{i:02}"), None),
                    &at,
                )
                .unwrap();
                opencrab_db::queries::insert_session_log_at(
                    &conn,
                    &log_row("system", None, &format!("system-row-{i:02}"), None),
                    &at,
                )
                .unwrap();
            }
        }
    }
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let reply = Arc::new(Mutex::new("ok\nNO_REPLY".to_string()));
    let mut router = opencrab_llm::router::LlmRouter::new();
    router.add_provider(Arc::new(CapturingProvider {
        requests: requests.clone(),
        reply: reply.clone(),
    }));
    state.llm_router.swap(router);
    Fixture {
        state,
        requests,
        reply,
    }
}

/// The session turn as the external gate drives it for `profile`.
async fn profile_turn(f: &Fixture, profile: TurnModelProfile) -> opencrab_core::EngineResult {
    let (system, name) = f.state.build_agent_context("a1", &CallerIdentity::Owner);
    let system = format!("{system}{}GATEWAY-CONTEXT", opencrab_llm_types::SYSTEM_SEGMENT_BREAK);
    run_session_turn(
        &f.state,
        "sess-a",
        "a1",
        profile,
        &system,
        "",
        |raw| raw.to_string(),
        |conversation| {
            RunRequest::new(
                "a1",
                name.clone(),
                "sess-a",
                system.clone(),
                conversation,
                "test",
                CallerIdentity::Owner,
            )
        },
    )
    .await
    .expect("turn ran")
    .expect("turn succeeded")
}

#[tokio::test]
async fn d1085_dump_default_request() {
    let f = fixture();
    profile_turn(&f, TurnModelProfile::Default).await;
    let requests = f.requests.lock().unwrap().clone();
    let _ = &f.reply;
    std::fs::write(
        std::env::var("D1085_DUMP").unwrap(),
        serde_json::to_string_pretty(&requests).unwrap(),
    )
    .unwrap();
}
