use std::io::{self, Write};
use std::sync::{Arc, Mutex};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("opencrab=info".parse()?),
        )
        .init();

    println!("OpenCrab CLI v0.1.0");
    println!("Type 'help' for commands, 'quit' to exit.\n");

    // DB初期化
    let conn = opencrab_db::init_connection("data/opencrab.db")?;
    let _db = Arc::new(Mutex::new(conn));

    loop {
        print!("opencrab> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();

        match input {
            "quit" | "exit" => {
                println!("Goodbye!");
                break;
            }
            "help" => {
                println!("Available commands:");
                println!("  agents list       - List all agents");
                println!("  agents create     - Create a new agent");
                println!("  sessions list     - List all sessions");
                println!("  sessions create   - Create a new session");
                println!("  help              - Show this help");
                println!("  quit              - Exit");
            }
            "agents list" => {
                let conn = _db.lock().unwrap();
                let mut stmt = conn
                    .prepare(
                        "SELECT i.agent_id, i.name, COALESCE(s.persona_name, '') FROM identity i LEFT JOIN soul s ON i.agent_id = s.agent_id",
                    )
                    .unwrap();
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })
                    .unwrap();
                for row in rows {
                    if let Ok((id, name, persona)) = row {
                        println!("  {} - {} ({})", id, name, persona);
                    }
                }
            }
            "sessions list" => {
                let conn = _db.lock().unwrap();
                let sessions = opencrab_db::queries::list_sessions(&conn).unwrap_or_default();
                for s in sessions {
                    println!(
                        "  {} - {} [{}] ({})",
                        s.id, s.theme, s.status, s.mode
                    );
                }
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
