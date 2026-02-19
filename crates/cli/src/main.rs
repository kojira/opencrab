use std::io::{self, Write};
use std::sync::{Arc, Mutex};

fn prompt(label: &str) -> String {
    print!("{}: ", label);
    io::stdout().flush().unwrap();
    let mut buf = String::new();
    io::stdin().read_line(&mut buf).unwrap();
    buf.trim().to_string()
}

/// Prompt with a default value shown in brackets.
fn prompt_default(label: &str, default: &str) -> String {
    let val = prompt(&format!("{} [{}]", label, default));
    if val.is_empty() { default.to_string() } else { val }
}

/// Resolve a partial agent ID or name to a single agent_id.
/// Returns None if no match or ambiguous.
fn resolve_agent(conn: &rusqlite::Connection, query: &str) -> Option<String> {
    let matches = opencrab_db::queries::find_agents(conn, query).unwrap_or_default();
    match matches.len() {
        0 => {
            println!("No agent found matching '{}'.", query);
            None
        }
        1 => Some(matches[0].0.clone()),
        _ => {
            println!("Ambiguous match for '{}'. Did you mean:", query);
            for (id, name) in &matches {
                println!("  {} - {}", &id[..8], name);
            }
            None
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("opencrab=info".parse()?),
        )
        .init();

    println!("OpenCrab CLI v0.1.0");
    println!("Type 'help' for commands, 'quit' to exit.\n");

    // Load config
    let cfg = opencrab_server::config::load_config("config/default.toml")?;

    // DB初期化
    let conn = opencrab_db::init_connection(&cfg.database.path)?;
    let db = Arc::new(Mutex::new(conn));

    // Build LLM router
    let _llm_router = opencrab_server::config::build_llm_router(&cfg.llm)?;

    loop {
        print!("opencrab> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();
        let parts: Vec<&str> = input.splitn(3, ' ').collect();

        match parts.as_slice() {
            ["quit" | "exit"] => {
                println!("Goodbye!");
                break;
            }
            ["help"] => {
                println!("Available commands:");
                println!("  agents list              - List all agents");
                println!("  agents create            - Create a new agent (interactive)");
                println!("  agents show <id|name>    - Show agent details");
                println!("  agents update <id|name>  - Update agent (interactive)");
                println!("  agents delete <id|name>  - Delete an agent");
                println!("  sessions list            - List all sessions");
                println!("  sessions create          - Create a new session (interactive)");
                println!("  help                     - Show this help");
                println!("  quit                     - Exit");
            }

            // ── agents list ──
            ["agents", "list"] => {
                let conn = db.lock().unwrap();
                let mut stmt = conn
                    .prepare(
                        "SELECT i.agent_id, i.name, i.role, COALESCE(s.persona_name, '') \
                         FROM identity i LEFT JOIN soul s ON i.agent_id = s.agent_id",
                    )
                    .unwrap();
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })
                    .unwrap();
                let mut count = 0;
                for row in rows {
                    if let Ok((id, name, role, persona)) = row {
                        println!("  {} - {} [{}] ({})", &id[..8], name, role, persona);
                        count += 1;
                    }
                }
                if count == 0 {
                    println!("  (no agents found)");
                }
            }

            // ── agents create ──
            ["agents", "create"] => {
                let name = prompt("Agent name");
                if name.is_empty() {
                    println!("Cancelled.");
                    continue;
                }
                let role = prompt_default("Role", "discussant");
                let persona = prompt("Persona name (e.g. Creative Researcher)");

                let agent_id = uuid::Uuid::new_v4().to_string();
                let conn = db.lock().unwrap();

                opencrab_db::queries::upsert_identity(
                    &conn,
                    &opencrab_db::queries::IdentityRow {
                        agent_id: agent_id.clone(),
                        name: name.clone(),
                        role,
                        job_title: None,
                        organization: None,
                        image_url: None,
                        metadata_json: None,
                    },
                )?;

                opencrab_db::queries::upsert_soul(
                    &conn,
                    &opencrab_db::queries::SoulRow {
                        agent_id: agent_id.clone(),
                        persona_name: if persona.is_empty() { name.clone() } else { persona },
                        social_style_json: "{}".to_string(),
                        personality_json: "{}".to_string(),
                        thinking_style_json: "{}".to_string(),
                        custom_traits_json: None,
                    },
                )?;

                println!("Created agent: {} ({})", name, &agent_id[..8]);
            }

            // ── agents show <query> ──
            ["agents", "show", query] => {
                let conn = db.lock().unwrap();
                if let Some(agent_id) = resolve_agent(&conn, query) {
                    let identity = opencrab_db::queries::get_identity(&conn, &agent_id)?;
                    let soul = opencrab_db::queries::get_soul(&conn, &agent_id)?;
                    let skills = opencrab_db::queries::list_skills(&conn, &agent_id, false)?;

                    if let Some(id) = identity {
                        println!("Agent: {}", id.name);
                        println!("  ID:           {}", id.agent_id);
                        println!("  Role:         {}", id.role);
                        if let Some(ref jt) = id.job_title {
                            println!("  Job title:    {}", jt);
                        }
                        if let Some(ref org) = id.organization {
                            println!("  Organization: {}", org);
                        }
                    }

                    if let Some(s) = soul {
                        println!("  Persona:      {}", s.persona_name);
                    }

                    if !skills.is_empty() {
                        println!("  Skills ({}):", skills.len());
                        for sk in &skills {
                            let status = if sk.is_active { "active" } else { "inactive" };
                            println!("    - {} [{}] (used {} times)", sk.name, status, sk.usage_count);
                        }
                    }
                }
            }
            ["agents", "show"] => {
                println!("Usage: agents show <id|name>");
            }

            // ── agents update <query> ──
            ["agents", "update", query] => {
                let agent_id = {
                    let conn = db.lock().unwrap();
                    resolve_agent(&conn, query)
                };

                if let Some(agent_id) = agent_id {
                    // Read current values
                    let (cur_name, cur_role, cur_persona) = {
                        let conn = db.lock().unwrap();
                        let identity = opencrab_db::queries::get_identity(&conn, &agent_id)?;
                        let soul = opencrab_db::queries::get_soul(&conn, &agent_id)?;
                        (
                            identity.as_ref().map(|i| i.name.clone()).unwrap_or_default(),
                            identity.as_ref().map(|i| i.role.clone()).unwrap_or_default(),
                            soul.as_ref().map(|s| s.persona_name.clone()).unwrap_or_default(),
                        )
                    };

                    println!("Updating agent: {} ({})", cur_name, &agent_id[..8]);
                    println!("Press Enter to keep current value.\n");

                    let name = prompt_default("Name", &cur_name);
                    let role = prompt_default("Role", &cur_role);
                    let persona = prompt_default("Persona", &cur_persona);

                    let conn = db.lock().unwrap();

                    // Re-read full rows to preserve other fields
                    let identity = opencrab_db::queries::get_identity(&conn, &agent_id)?;
                    if let Some(mut id) = identity {
                        id.name = name.clone();
                        id.role = role;
                        opencrab_db::queries::upsert_identity(&conn, &id)?;
                    }

                    let soul = opencrab_db::queries::get_soul(&conn, &agent_id)?;
                    if let Some(mut s) = soul {
                        s.persona_name = persona;
                        opencrab_db::queries::upsert_soul(&conn, &s)?;
                    }

                    println!("Updated agent: {}", name);
                }
            }
            ["agents", "update"] => {
                println!("Usage: agents update <id|name>");
            }

            // ── agents delete <query> ──
            ["agents", "delete", query] => {
                let conn = db.lock().unwrap();
                if let Some(agent_id) = resolve_agent(&conn, query) {
                    let identity = opencrab_db::queries::get_identity(&conn, &agent_id)?;
                    let name = identity.map(|i| i.name).unwrap_or_else(|| agent_id[..8].to_string());
                    drop(conn); // release lock for prompt

                    let confirm = prompt(&format!("Delete agent '{}'? (yes/no)", name));
                    if confirm == "yes" || confirm == "y" {
                        let conn = db.lock().unwrap();
                        let deleted = opencrab_db::queries::delete_agent(&conn, &agent_id)?;
                        if deleted {
                            println!("Deleted agent: {}", name);
                        } else {
                            println!("Agent not found.");
                        }
                    } else {
                        println!("Cancelled.");
                    }
                }
            }
            ["agents", "delete"] => {
                println!("Usage: agents delete <id|name>");
            }

            // ── sessions list ──
            ["sessions", "list"] => {
                let conn = db.lock().unwrap();
                let sessions = opencrab_db::queries::list_sessions(&conn).unwrap_or_default();
                if sessions.is_empty() {
                    println!("  (no sessions found)");
                } else {
                    for s in sessions {
                        println!(
                            "  {} - {} [{}] ({})",
                            &s.id[..8], s.theme, s.status, s.mode
                        );
                    }
                }
            }

            // ── sessions create ──
            ["sessions", "create"] => {
                let theme = prompt("Discussion theme");
                if theme.is_empty() {
                    println!("Cancelled.");
                    continue;
                }
                let mode = prompt_default("Mode (autonomous/mentored)", "autonomous");
                let max_turns_str = prompt_default("Max turns", "10");
                let max_turns: i32 = max_turns_str.parse().unwrap_or(10);

                // List agents for participant selection
                let agents: Vec<(String, String)> = {
                    let conn = db.lock().unwrap();
                    let mut stmt = conn
                        .prepare("SELECT agent_id, name FROM identity")
                        .unwrap();
                    stmt.query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect()
                };

                if agents.is_empty() {
                    println!("No agents available. Create agents first with 'agents create'.");
                    continue;
                }

                println!("Available agents:");
                for (i, (id, name)) in agents.iter().enumerate() {
                    println!("  {}. {} ({})", i + 1, name, &id[..8]);
                }

                let selection = prompt_default("Select participants (comma-separated numbers)", "all");
                let participant_ids: Vec<String> = if selection == "all" {
                    agents.iter().map(|(id, _)| id.clone()).collect()
                } else {
                    selection
                        .split(',')
                        .filter_map(|s| {
                            let idx: usize = s.trim().parse().ok()?;
                            agents.get(idx.wrapping_sub(1)).map(|(id, _)| id.clone())
                        })
                        .collect()
                };

                if participant_ids.is_empty() {
                    println!("No valid participants selected. Cancelled.");
                    continue;
                }

                let session_id = uuid::Uuid::new_v4().to_string();
                let conn = db.lock().unwrap();
                opencrab_db::queries::insert_session(
                    &conn,
                    &opencrab_db::queries::SessionRow {
                        id: session_id.clone(),
                        mode,
                        theme: theme.clone(),
                        phase: "divergent".to_string(),
                        turn_number: 0,
                        status: "active".to_string(),
                        participant_ids_json: serde_json::to_string(&participant_ids).unwrap(),
                        facilitator_id: None,
                        done_count: 0,
                        max_turns: Some(max_turns),
                        metadata_json: None,
                    },
                )?;

                println!(
                    "Created session: {} ({}) with {} participants",
                    theme,
                    &session_id[..8],
                    participant_ids.len()
                );
            }

            _ => {
                if !input.is_empty() {
                    println!("Unknown command: {}. Type 'help' for available commands.", input);
                }
            }
        }
    }

    Ok(())
}
