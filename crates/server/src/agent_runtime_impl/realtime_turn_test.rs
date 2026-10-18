#![cfg(test)]
//! D-1085 P1: a realtime-profile session turn is a lightweight single generation; a
//! default-profile turn is unchanged.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opencrab_actions::{
    start_session_turn, AgentRuntime, CallerIdentity, NormalizedInbound, RunRequest,
    TranscriptSource, TurnModelProfile,
};
use opencrab_llm::message::{ChatRequest, ChatResponse, Choice, FinishReason, Message};
use opencrab_llm::traits::{LlmProvider, ModelInfo};

type Requests = Arc<Mutex<Vec<ChatRequest>>>;

/// Provider `pa` with model `small`; records every request and answers with the next scripted
/// message, else with `reply`.
struct CapturingProvider {
    requests: Requests,
    reply: Arc<Mutex<String>>,
    script: Arc<Mutex<Vec<Message>>>,
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
        let scripted = {
            let mut script = self.script.lock().unwrap();
            (!script.is_empty()).then(|| script.remove(0))
        };
        let message =
            scripted.unwrap_or_else(|| Message::assistant(self.reply.lock().unwrap().clone()));
        Ok(ChatResponse {
            id: "r".to_string(),
            model: request.model,
            choices: vec![Choice {
                index: 0,
                message,
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
    script: Arc<Mutex<Vec<Message>>>,
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
                    &log_row(
                        "tool_result",
                        Some("a1"),
                        &format!("tool-result-{i:02}"),
                        None,
                    ),
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
    let script = Arc::new(Mutex::new(Vec::new()));
    let mut router = opencrab_llm::router::LlmRouter::new();
    router.add_provider(Arc::new(CapturingProvider {
        requests: requests.clone(),
        reply: reply.clone(),
        script: script.clone(),
    }));
    state.llm_router.swap(router);
    Fixture {
        state,
        requests,
        reply,
        script,
    }
}

/// The inbound session turn as the external gate drives it for `profile`: a realtime said takes the
/// realtime system prompt, any other the full agent context; the gateway context is appended.
async fn profile_turn(f: &Fixture, profile: TurnModelProfile) -> opencrab_core::EngineResult {
    profile_turn_with(f, profile, None).await
}

/// [`profile_turn`] with the gateway's declared operations.
async fn profile_turn_with(
    f: &Fixture,
    profile: TurnModelProfile,
    gateway_actions: Option<Arc<dyn opencrab_gateway::GatewayActions>>,
) -> opencrab_core::EngineResult {
    let (system, name) = f.state.build_agent_context("a1", &CallerIdentity::Owner);
    let system = format!(
        "{system}{}GATEWAY-CONTEXT",
        opencrab_llm_types::SYSTEM_SEGMENT_BREAK
    );
    let inbound = NormalizedInbound {
        session_id: "sess-a",
        agent_id: "a1",
        sender_id: "u1",
        sender_name: "",
        avatar_url: None,
        channel_id: None,
        pubkey: None,
        text: "hi",
        image_urls: &[],
        external_id: "origin-1",
    };
    start_session_turn(
        &f.state,
        TranscriptSource::new("external", "external_response"),
        &inbound,
        profile,
        &system,
        "",
        |raw| raw.to_string(),
        |conversation| {
            let req = RunRequest::new(
                "a1",
                name.clone(),
                "sess-a",
                system.clone(),
                conversation,
                "test",
                CallerIdentity::Owner,
            );
            match gateway_actions {
                Some(ga) => req.with_gateway_actions(ga),
                None => req,
            }
        },
    )
    .await
    .expect("turn ran")
    .expect("turn succeeded")
}

fn system_text(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter(|m| m.role == opencrab_llm::message::Role::System)
        .filter_map(|m| m.text_content())
        .collect()
}

fn user_text(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter(|m| m.role == opencrab_llm::message::Role::User)
        .filter_map(|m| m.text_content())
        .collect()
}

/// L1/L3 (default side): a default-profile turn keeps the full prompt, tools, effort and web
/// search. Set `D1085_DUMP` to write the captured requests for a byte comparison.
#[tokio::test]
async fn default_profile_turn_keeps_the_full_request() {
    let f = fixture();
    profile_turn(&f, TurnModelProfile::Default).await;
    let requests = f.requests.lock().unwrap().clone();
    if let Ok(path) = std::env::var("D1085_DUMP") {
        std::fs::write(path, serde_json::to_string_pretty(&requests).unwrap()).unwrap();
    }
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let system = system_text(request);
    assert!(system.contains("## Long-term Memory"));
    assert!(system.contains("SKILL-DESCRIPTION"));
    assert!(system.contains("INSTRUCTIONS-BODY"));
    assert!(system.contains("## More tools"));
    assert!(request.functions.as_ref().is_some_and(|f| !f.is_empty()));
    assert_eq!(request.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(
        request.metadata.get("web_search"),
        Some(&serde_json::json!(true))
    );
}

/// The system prompt without its More-tools index (the index ends at the next segment break).
fn without_more_tools(system: &str) -> String {
    let Some(at) = system.find("\n\n## More tools") else {
        return system.to_string();
    };
    let end = system[at..]
        .find(opencrab_llm_types::SYSTEM_SEGMENT_BREAK)
        .map_or(system.len(), |e| at + e);
    format!("{}{}", &system[..at], &system[end..])
}

/// The tool names of the More-tools index lines (`- <category>: a, b`).
fn more_tools_names(system: &str) -> Vec<String> {
    let Some(at) = system.find("\n\n## More tools") else {
        return Vec::new();
    };
    let end = system[at..]
        .find(opencrab_llm_types::SYSTEM_SEGMENT_BREAK)
        .map_or(system.len(), |e| at + e);
    let mut names: Vec<String> = system[at..end]
        .lines()
        .filter_map(|line| line.strip_prefix("- "))
        .filter_map(|line| line.split_once(": "))
        .flat_map(|(_, names)| names.split(", ").map(str::to_string))
        .collect();
    names.sort();
    names
}

/// §13.1 (a): a realtime turn sends one request whose system prompt is the default turn's (same
/// builder, same agent and caller) apart from the More-tools index, without the agent effort; a
/// text without the marker is the final response. Web search follows the agent setting.
#[tokio::test]
async fn realtime_turn_has_the_system_prompt_of_a_default_turn() {
    let f = fixture();
    profile_turn(&f, TurnModelProfile::Default).await;
    *f.reply.lock().unwrap() = "了解。".to_string();
    let result = profile_turn(&f, TurnModelProfile::Realtime).await;
    let requests = f.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "one LLM call per turn");
    assert_eq!(result.response, "了解。");
    assert!(result.explicit_termination.is_none());

    let (default, realtime) = (&requests[0], &requests[1]);
    assert_eq!(
        without_more_tools(&system_text(realtime)),
        without_more_tools(&system_text(default))
    );
    assert!(system_text(realtime).contains("INSTRUCTIONS-BODY"));
    assert!(realtime.reasoning_effort.is_none());
    assert_eq!(
        realtime.metadata.get("web_search"),
        Some(&serde_json::json!(true))
    );
}

fn function_names(request: &ChatRequest) -> Vec<String> {
    request
        .functions
        .iter()
        .flatten()
        .map(|f| f.name.clone())
        .collect()
}

/// §13.1 (b): a realtime turn offers exactly the lightweight allowlist plus the operations its
/// gateway declares: projected or listed in its More-tools index, and nothing else; describe_tools
/// loads only those. A default turn is not limited.
#[tokio::test]
async fn realtime_turn_tools_are_the_gateway_operations_and_the_lightweight_allowlist() {
    let f = fixture();
    let (ops, _) = recording_ops();
    profile_turn_with(&f, TurnModelProfile::Default, Some(ops.clone())).await;
    profile_turn_with(&f, TurnModelProfile::Realtime, Some(ops)).await;
    let requests = f.requests.lock().unwrap().clone();
    let offered = |r: &ChatRequest| {
        let mut names = function_names(r);
        names.extend(more_tools_names(&system_text(r)));
        names.sort();
        names.dedup();
        names
    };
    let mut expected: Vec<String> = opencrab_actions::LIGHTWEIGHT_TURN_TOOLS
        .iter()
        .map(|n| n.to_string())
        .chain(["gw_act".to_string()])
        .collect();
    expected.sort();
    assert_eq!(offered(&requests[1]), expected);
    let default = offered(&requests[0]);
    assert!(default.len() > expected.len(), "{default:?}");
    assert!(
        default.iter().any(|n| n == "create_my_skill"),
        "{default:?}"
    );
    assert!(expected.iter().all(|n| default.contains(n)), "{default:?}");
    eprintln!(
        "D-1085 §13.1 fixture tools: realtime_functions={} realtime_functions_json_chars={} \
         default_functions={} default_functions_json_chars={}",
        function_names(&requests[1]).len(),
        serde_json::to_string(&requests[1].functions)
            .unwrap()
            .chars()
            .count(),
        function_names(&requests[0]).len(),
        serde_json::to_string(&requests[0].functions)
            .unwrap()
            .chars()
            .count()
    );
}

/// The text of the user message before `<conversation_history>` and the history body.
fn split_history(user: &str) -> (&str, &str) {
    let open = user
        .find(opencrab_core::conversation::CONVERSATION_HISTORY_START)
        .expect("history start");
    let close = user
        .find(opencrab_core::conversation::CONVERSATION_HISTORY_END)
        .expect("history end");
    let body = &user[open + opencrab_core::conversation::CONVERSATION_HISTORY_START.len()..close];
    (&user[..open], body.trim_matches('\n'))
}

/// §13.1 (c): the realtime conversation is the normal rendering of every retained row (speech,
/// tool and system) from the 30th most recent speech row on, and nothing before the history:
/// no task ledger, memory index or impressions.
#[tokio::test]
async fn realtime_conversation_is_the_recent_window_without_prefix_sections() {
    let f = fixture();
    {
        let conn = f.state.db.lock().unwrap();
        opencrab_db::queries::insert_task_ledger(&conn, "a1", "sess-a", "LEDGER-GOAL", None)
            .unwrap();
    }
    // The same session through the normal builder, before the turn.
    let normal = {
        let conn = f.state.db.lock().unwrap();
        opencrab_core::conversation::build_conversation_string(&conn, "sess-a", "a1", 200_000)
            .unwrap()
    };
    profile_turn(&f, TurnModelProfile::Realtime).await;
    let requests = f.requests.lock().unwrap().clone();
    let realtime_user = user_text(&requests[0]);
    let (normal_prefix, normal_body) = split_history(&normal);
    let (realtime_prefix, realtime_body) = split_history(&realtime_user);
    assert!(normal_prefix.contains("LEDGER-GOAL"), "{normal_prefix}");
    assert_eq!(realtime_prefix, "");
    assert!(!realtime_user.contains("LEDGER-GOAL"));

    let first = normal_body
        .find("user-speech-10")
        .and_then(|at| normal_body[..at].rfind("\n[").map(|line| line + 1))
        .expect("header of user-speech-10");
    assert_eq!(realtime_body, &normal_body[first..]);
    for present in ["tool-result-15", "tool-result-35", "system-row-15"] {
        assert!(
            realtime_body.contains(present),
            "{present}: {realtime_body}"
        );
    }
    for absent in ["agent-speech-09", "tool-result-05", "system-row-05"] {
        assert!(!realtime_body.contains(absent), "{absent}: {realtime_body}");
    }
    eprintln!(
        "D-1085 §13.1 fixture sizes: system_chars={} user_chars={} history_chars={}",
        system_text(&requests[0]).chars().count(),
        realtime_user.chars().count(),
        realtime_body.chars().count()
    );
}

/// L5: a bare NO_REPLY ends a realtime turn silently after one call.
#[tokio::test]
async fn realtime_bare_no_reply_is_silent_after_one_call() {
    let f = fixture();
    *f.reply.lock().unwrap() = "NO_REPLY".to_string();
    let result = profile_turn(&f, TurnModelProfile::Realtime).await;
    assert_eq!(f.requests.lock().unwrap().len(), 1);
    assert_eq!(result.response, "");
    assert_eq!(
        result.explicit_termination,
        Some(opencrab_core::ExplicitTermination::NoReply)
    );
    let effect = opencrab_actions::delivery_effect(
        Ok(result),
        opencrab_actions::DeliveryContext {
            session_id: "sess-a",
            agent_id: "a1",
            origin: "test",
        },
    );
    assert!(matches!(effect, opencrab_actions::DeliveryEffect::NoReply));
}

/// D-1085 §9: a said's agent prelude (recorded by the external gate) is the agent's own last
/// line in both the realtime and the default conversation, right after the said, and nothing
/// follows it.
#[tokio::test]
async fn agent_prelude_is_the_agent_line_after_the_said_in_both_histories() {
    for profile in [TurnModelProfile::Realtime, TurnModelProfile::Default] {
        let f = fixture();
        {
            let conn = f.state.db.lock().unwrap();
            opencrab_db::queries::insert_session_log_at(
                &conn,
                &log_row("speech", Some("u1"), "user-said-now", None),
                "2026-10-18T11:00:00+00:00",
            )
            .unwrap();
            opencrab_extgate::inbound::record_agent_prelude(&conn, "sess-a", "a1", "PRELUDE-UN")
                .unwrap();
        }
        profile_turn(&f, profile).await;
        let requests = f.requests.lock().unwrap().clone();
        let user = user_text(&requests[0]);
        let history = user
            .split(opencrab_core::conversation::CONVERSATION_HISTORY_END)
            .next()
            .unwrap();
        assert!(
            history.contains("PRELUDE-UN"),
            "{profile:?}: prelude in history: {history}"
        );
        let said = history.find("user-said-now").expect("said in history");
        let prelude = history.find("PRELUDE-UN").unwrap();
        assert!(said < prelude, "{profile:?}: {history}");
        let header = history[..prelude].trim_end().lines().last().unwrap();
        assert!(
            header.starts_with("[Kani]"),
            "{profile:?}: the prelude is the agent's line: {history}"
        );
        assert_eq!(
            history[prelude..].trim_end(),
            "PRELUDE-UN",
            "{profile:?}: nothing after the prelude: {history}"
        );
    }
}

// D-1085 P5: provisional runs share this fixture.
include!("provisional_turn_test.rs");
// D-1085 §12: tools of lightweight and provisional runs.
include!("provisional_gate_test.rs");
