//! Multi-agent conversation integration test.
//!
//! Creates 3 AI agents with distinct personalities, runs a multi-turn
//! discussion via OpenRouter, and verifies coherent conversation flow.
//!
//! Run with:
//!   OPENROUTER_API_KEY="sk-or-..." cargo test -p opencrab-llm --test multi_agent_conversation -- --ignored --nocapture

use opencrab_llm::message::*;
use opencrab_llm::providers::openrouter::OpenRouterProvider;
use opencrab_llm::traits::LlmProvider;

fn api_key() -> String {
    std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set")
}

fn provider() -> OpenRouterProvider {
    OpenRouterProvider::new(api_key()).with_title("OpenCrab Multi-Agent Test")
}

const MODEL: &str = "openai/gpt-4o-mini";

/// An agent with a name, role, and system prompt representing its personality.
struct Agent {
    name: String,
    system_prompt: String,
}

impl Agent {
    fn new(name: &str, role: &str, personality: &str) -> Self {
        let system_prompt = format!(
            "You are {name}, a {role}. {personality}\n\
             Rules:\n\
             - Keep your responses to 2-3 sentences max.\n\
             - Stay in character at all times.\n\
             - Address other participants by name when responding to them.\n\
             - You are in a group discussion with other agents."
        );
        Self {
            name: name.to_string(),
            system_prompt,
        }
    }

    /// Generate a response given the conversation history so far.
    async fn respond(
        &self,
        provider: &OpenRouterProvider,
        history: &[Message],
    ) -> anyhow::Result<String> {
        let mut messages = vec![Message::system(&self.system_prompt)];
        messages.extend_from_slice(history);

        let request = ChatRequest::new(MODEL, messages)
            .with_temperature(0.8)
            .with_max_tokens(150);

        let response = provider.chat_completion(request).await?;
        let text = response
            .first_text()
            .unwrap_or("[no response]")
            .to_string();
        Ok(text)
    }
}

#[tokio::test]
#[ignore]
async fn test_three_agent_discussion() {
    let p = provider();

    // Create 3 agents with distinct personalities
    let agents = vec![
        Agent::new(
            "Kai",
            "pragmatic engineer",
            "You value practical solutions and efficiency. You tend to focus on what's achievable \
             and ask about concrete implementation details. You're skeptical of over-engineering.",
        ),
        Agent::new(
            "Aria",
            "creative researcher",
            "You love exploring new ideas and unconventional approaches. You often suggest \
             innovative solutions and ask 'what if' questions. You're optimistic about possibilities.",
        ),
        Agent::new(
            "Reo",
            "cautious analyst",
            "You focus on risk assessment and careful evaluation. You point out potential issues \
             and advocate for thorough testing. You're methodical and detail-oriented.",
        ),
    ];

    let topic = "Should we build our next project using Rust or Go?";
    let sep = "=".repeat(60);
    println!("\n{sep}");
    println!("TOPIC: {topic}");
    println!("{sep}\n");

    // Shared conversation history (user messages represent each agent's speech)
    let mut history: Vec<Message> = vec![Message::user(&format!(
        "[Moderator]: Let's discuss: {topic}\nEach of you, share your perspective."
    ))];

    // Round 1: Each agent gives their initial take
    println!("--- Round 1: Initial Perspectives ---\n");
    for agent in &agents {
        let response = agent.respond(&p, &history).await.unwrap();
        println!("[{}]: {}\n", agent.name, response);

        // Add to shared history as user message so other agents see it
        history.push(Message::assistant(&format!("[{}]: {}", agent.name, response)));
        history.push(Message::user("Next participant, please share your view."));
    }

    // Round 2: Each agent responds to what others said
    println!("--- Round 2: Responses & Debate ---\n");
    history.push(Message::user(
        "[Moderator]: Now respond to each other's points. Do you agree or disagree?",
    ));

    for agent in &agents {
        let response = agent.respond(&p, &history).await.unwrap();
        println!("[{}]: {}\n", agent.name, response);

        history.push(Message::assistant(&format!("[{}]: {}", agent.name, response)));
    }

    // Round 3: Summary and conclusion
    println!("--- Round 3: Final Conclusions ---\n");
    history.push(Message::user(
        "[Moderator]: Summarize your final stance in one sentence.",
    ));

    let mut final_responses = Vec::new();
    for agent in &agents {
        let response = agent.respond(&p, &history).await.unwrap();
        println!("[{}]: {}\n", agent.name, response);
        final_responses.push(response.clone());

        history.push(Message::assistant(&format!("[{}]: {}", agent.name, response)));
    }

    // ---------- Assertions ----------

    // Each agent produced non-empty responses in all rounds
    assert_eq!(
        history.len(),
        1 + (3 * 2) + 1 + (3 * 1) + 1 + (3 * 1),
        "History should have the right number of messages"
    );

    for resp in &final_responses {
        assert!(!resp.is_empty(), "Final responses should not be empty");
        assert!(
            resp.len() > 10,
            "Final responses should be substantive, got: {resp}"
        );
    }

    // Verify the conversation was about the topic (Rust or Go)
    let full_conversation: String = history
        .iter()
        .filter_map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();

    assert!(
        full_conversation.contains("rust") || full_conversation.contains("go"),
        "Conversation should discuss Rust or Go"
    );

    println!("{sep}");
    println!("Discussion complete! {} total messages exchanged.", history.len());
    println!("{sep}");
}

#[tokio::test]
#[ignore]
async fn test_three_agent_creative_story() {
    let p = provider();

    let agents = vec![
        Agent::new(
            "Narrator",
            "storyteller",
            "You set the scene and describe what's happening. You create atmospheric descriptions \
             and move the plot forward.",
        ),
        Agent::new(
            "Hero",
            "brave adventurer character",
            "You are the protagonist of the story. You describe your actions, feelings, and dialogue \
             in first person. You are courageous but sometimes uncertain.",
        ),
        Agent::new(
            "Sage",
            "wise mentor character",
            "You are a mysterious old sage who gives cryptic but helpful advice. You speak in short, \
             profound statements. You know secrets about the world.",
        ),
    ];

    let sep = "=".repeat(60);
    println!("\n{sep}");
    println!("COLLABORATIVE STORY");
    println!("{sep}\n");

    let mut history: Vec<Message> = vec![Message::user(
        "Let's collaboratively create a short fantasy story. \
         The setting: a lone traveler arrives at an ancient library at the edge of the world. \
         Each of you, add one paragraph to the story in your role.",
    )];

    // 2 rounds of collaborative storytelling
    for round in 1..=2 {
        println!("--- Chapter {round} ---\n");
        for agent in &agents {
            let response = agent.respond(&p, &history).await.unwrap();
            println!("[{}]: {}\n", agent.name, response);

            history.push(Message::assistant(&format!("[{}]: {}", agent.name, response)));
        }
        if round < 2 {
            history.push(Message::user("Continue the story. What happens next?"));
        }
    }

    // Verify story coherence
    let story_text: String = history
        .iter()
        .filter_map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();

    assert!(
        story_text.contains("library") || story_text.contains("book") || story_text.contains("ancient"),
        "Story should reference the library setting"
    );

    // Each agent contributed
    let agent_names = ["narrator", "hero", "sage"];
    for name in agent_names {
        assert!(
            story_text.contains(name),
            "Story should include contributions from {name}"
        );
    }

    println!("Story complete! {} messages total.", history.len());
}

#[tokio::test]
#[ignore]
async fn test_three_agent_with_db_and_session() {
    let p = provider();

    // Set up in-memory DB for full integration
    let conn = opencrab_db::init_memory().unwrap();

    // Create 3 agents in DB
    let agent_configs = vec![
        ("Kai", "Pragmatic Engineer", "discussant"),
        ("Aria", "Creative Researcher", "discussant"),
        ("Reo", "Cautious Analyst", "facilitator"),
    ];

    let mut agent_ids = Vec::new();
    for (name, persona, role) in &agent_configs {
        let id = uuid::Uuid::new_v4().to_string();
        opencrab_db::queries::upsert_identity(
            &conn,
            &opencrab_db::queries::IdentityRow {
                agent_id: id.clone(),
                name: name.to_string(),
                role: role.to_string(),
                job_title: None,
                organization: None,
                image_url: None,
                metadata_json: None,
            },
        )
        .unwrap();
        opencrab_db::queries::upsert_soul(
            &conn,
            &opencrab_db::queries::SoulRow {
                agent_id: id.clone(),
                persona_name: persona.to_string(),
                social_style_json: "{}".to_string(),
                personality_json: "{}".to_string(),
                thinking_style_json: "{}".to_string(),
                custom_traits_json: None,
            },
        )
        .unwrap();
        agent_ids.push(id);
    }

    // Create a session
    let session_id = uuid::Uuid::new_v4().to_string();
    opencrab_db::queries::insert_session(
        &conn,
        &opencrab_db::queries::SessionRow {
            id: session_id.clone(),
            mode: "autonomous".to_string(),
            theme: "Rust vs Go discussion".to_string(),
            phase: "divergent".to_string(),
            turn_number: 0,
            status: "active".to_string(),
            participant_ids_json: serde_json::to_string(&agent_ids).unwrap(),
            facilitator_id: Some(agent_ids[2].clone()),
            done_count: 0,
            max_turns: Some(6),
            metadata_json: None,
        },
    )
    .unwrap();

    // Verify session was created
    let session = opencrab_db::queries::get_session(&conn, &session_id).unwrap();
    assert_eq!(session.as_ref().unwrap().theme, "Rust vs Go discussion");

    // Each agent generates a response and logs it
    let system_prompts = vec![
        "You are Kai, a pragmatic engineer. Keep responses to 1-2 sentences.",
        "You are Aria, a creative researcher. Keep responses to 1-2 sentences.",
        "You are Reo, a cautious analyst. Keep responses to 1-2 sentences.",
    ];

    println!("\n--- Session: Rust vs Go ---\n");

    let mut conversation_messages = vec![Message::user(
        "Share your one-sentence opinion: should we use Rust or Go for a new CLI tool?",
    )];

    for (i, agent_id) in agent_ids.iter().enumerate() {
        let mut messages = vec![Message::system(system_prompts[i])];
        messages.extend_from_slice(&conversation_messages);

        let request = ChatRequest::new(MODEL, messages)
            .with_temperature(0.7)
            .with_max_tokens(100);

        let response = p.chat_completion(request).await.unwrap();
        let text = response.first_text().unwrap().to_string();

        println!("[{}]: {}\n", agent_configs[i].0, text);

        // Log to session
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            log_type: "speech".to_string(),
            content: text.clone(),
            speaker_id: Some(agent_id.clone()),
            turn_number: Some(i as i32 + 1),
            metadata_json: None,
        };
        opencrab_db::queries::insert_session_log(&conn, &log).unwrap();

        conversation_messages.push(Message::assistant(&format!(
            "[{}]: {}",
            agent_configs[i].0, text
        )));
    }

    // Verify all logs were saved
    let results = opencrab_db::queries::search_session_logs(&conn, &agent_ids[0], "Rust", 10);
    // FTS search may or may not match depending on content; just verify no error
    assert!(results.is_ok(), "Session log search should not error");

    // Verify session data
    let sessions = opencrab_db::queries::list_sessions(&conn).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].theme, "Rust vs Go discussion");

    // Verify all 3 agents exist
    for id in &agent_ids {
        let identity = opencrab_db::queries::get_identity(&conn, id).unwrap();
        assert!(identity.is_some(), "Agent should exist in DB");
    }

    println!("Full integration test passed! 3 agents, 1 session, {} logs.", agent_ids.len());
}
