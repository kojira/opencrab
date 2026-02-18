# OpenCrab

An autonomous AI agent framework built in Rust. Create, manage, and run AI agents with rich personality systems, skill management, multi-provider LLM routing, and multi-channel communication.

## Features

- **Multi-Provider LLM Support** -- OpenAI, Anthropic, Google Gemini, OpenRouter, Ollama, llama.cpp with intelligent routing and automatic fallback
- **Agent Personality System** -- Big Five traits, social styles, and thinking preferences via the Soul/Identity model
- **Memory Management** -- Curated memories and session logs with SQLite FTS5 full-text search
- **Skill System** -- Standard and acquired skills with effectiveness tracking and usage metrics
- **Multi-Channel Communication** -- REST API, CLI, WebSocket, and Discord gateway adapters
- **Sandboxed Workspace** -- Per-agent file operations with path traversal protection
- **Heartbeat Loop** -- Periodic autonomous agent activities
- **Self-Learning** -- Capability acquisition, response evaluation, and LLM usage analysis
- **Cost Tracking** -- Token usage, latency, and estimated cost per model

## Architecture

```
opencrab/
├── crates/
│   ├── core/       # Agent engine, soul, identity, memory, skills, workspace
│   ├── llm/        # Multi-provider LLM abstraction, routing, metrics, pricing
│   ├── gateway/    # Multi-channel message gateway (REST, CLI, WebSocket, Discord)
│   ├── actions/    # Action dispatcher and skill handlers
│   ├── db/         # SQLite persistence with FTS5 full-text search
│   ├── server/     # Axum REST API server
│   └── cli/        # Interactive REPL CLI
├── dashboard/      # Dioxus web UI
├── config/         # Configuration files
└── skills/         # Skill definition files
```

## Prerequisites

- [Rust](https://rustup.rs/) (edition 2021)
- SQLite (bundled via `rusqlite`)

## Getting Started

### 1. Clone and build

```bash
git clone https://github.com/yourname/opencrab.git
cd opencrab
cargo build
```

### 2. Set environment variables

```bash
export OPENAI_API_KEY="sk-..."
export ANTHROPIC_API_KEY="sk-ant-..."
# Optional
export GOOGLE_API_KEY="..."
export OPENROUTER_API_KEY="..."
export DISCORD_TOKEN="..."
```

### 3. Run the server

```bash
cargo run -p opencrab-server
# Listening on 0.0.0.0:8080
```

### 4. Or use the CLI

```bash
cargo run -p opencrab-cli
```

### 5. Run the dashboard

```bash
dx serve --project dashboard
# Listening on localhost:3000
```

## API Endpoints

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Health check |
| GET / POST | `/api/agents` | List / create agents |
| GET / DELETE | `/api/agents/{id}` | Get / delete agent |
| GET / PUT | `/api/agents/{id}/soul` | Get / update soul |
| GET / PUT | `/api/agents/{id}/identity` | Get / update identity |
| GET / POST | `/api/agents/{id}/skills` | List / add skills |
| POST | `/api/agents/{id}/skills/{skill_id}/toggle` | Toggle skill |
| GET | `/api/agents/{id}/memory/curated` | List curated memories |
| POST | `/api/agents/{id}/memory/search` | Search memory |
| GET / POST | `/api/sessions` | List / create sessions |
| GET | `/api/sessions/{id}` | Get session |
| POST | `/api/sessions/{id}/messages` | Send message |
| GET | `/api/agents/{id}/workspace` | List workspace files |
| GET / PUT | `/api/agents/{id}/workspace/*path` | Read / write file |

## Configuration

Configuration is loaded from `config/default.toml`:

- **LLM providers** -- Default provider, per-use-case model selection, fallback chains
- **Agent settings** -- Heartbeat interval, workspace path, max workspace size
- **Gateway settings** -- REST port, Discord token, CLI and dashboard toggles

## Testing

```bash
# Run all tests (~107 tests)
cargo test --workspace

# Run tests for a specific crate
cargo test -p opencrab-db
cargo test -p opencrab-llm
cargo test -p opencrab-core
cargo test -p opencrab-gateway
cargo test -p opencrab-actions

# Run E2E API tests
cargo test -p opencrab-server
```

## Tech Stack

| Component | Technology |
|-----------|------------|
| Language | Rust (edition 2021) |
| Async Runtime | Tokio |
| Web Framework | Axum |
| Database | SQLite (rusqlite) with FTS5 |
| HTTP Client | reqwest |
| Frontend | Dioxus + Tailwind CSS |
| Serialization | serde / serde_json |
| Error Handling | anyhow / thiserror |
| Logging | tracing / tracing-subscriber |

## License

MIT
