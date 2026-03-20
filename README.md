<p align="center">
  <img src="docs/icon.png" alt="OpenCrab" width="128" />
</p>

<h1 align="center">OpenCrab</h1>

<p align="center">
  An autonomous AI agent framework built in Rust. Create, manage, and run AI agents with rich personality systems, skill management, multi-provider LLM routing, and multi-channel communication.
</p>

## Features

- **Multi-Provider LLM Support** -- OpenAI, Anthropic, Google Gemini, OpenRouter, Ollama, llama.cpp with intelligent routing and automatic fallback
- **Agent Personality System** -- Big Five traits, social styles, and thinking preferences via the Soul/Identity model with saveable presets
- **Memory Management** -- Curated memories, session logs with FTS5 search, and hierarchical memory index with LLM-powered Agentic RAG
- **Skill System** -- Standard and acquired skills with effectiveness tracking, usage metrics, and bridge executor connecting ActionDispatcher to SkillEngine
- **Multi-Channel Communication** -- REST API, CLI, WebSocket, and Discord gateway adapters
- **Per-Agent Discord Gateway** -- DB-persisted Discord config per agent with independent start/stop lifecycle management
- **Co-Agent Management** -- Trust relationships between agents with configurable permission levels (owner/agent/co-agent)
- **Trusted User Whitelist** -- Per-agent Discord user trust management
- **Sandboxed Workspace** -- Per-agent file operations with path traversal protection
- **Heartbeat Loop** -- Periodic autonomous agent activity with prime-numbered interval (default 29s) and tokio::watch-based graceful shutdown
- **Self-Learning** -- Experience-based learning, peer learning, reflection, and skill creation
- **LLM Self-Selection** -- Agents dynamically select LLMs per task based on past experience
- **Response Evaluation** -- Quality scoring after each interaction
- **Cost Tracking** -- Token usage, latency, and estimated cost per model
- **Mentor Instruction** -- Send instructions to agents as a privileged "mentor" role
- **Hot-Reload Configuration** -- `config/` directory watched with `notify_debouncer_mini`; ToolsConfig live-updates without restart
- **i18n Dashboard** -- React frontend with English and Japanese localization

## Architecture

```
opencrab/
├── crates/
│   ├── core/       # Agent engine, soul, identity, memory, skills, workspace, heartbeat
│   ├── llm/        # Multi-provider LLM abstraction, routing, metrics, pricing
│   ├── gateway/    # Multi-channel message gateway (REST, CLI, WebSocket, Discord)
│   ├── actions/    # Action dispatcher (27 actions) and skill bridge executor
│   ├── db/         # SQLite persistence with FTS5 full-text search
│   ├── server/     # Axum REST API server with hot-reload config watcher
│   ├── cli/        # Interactive REPL CLI
│   └── discord/    # Discord gateway with per-agent manager and message loop
├── web/            # React frontend (Vite + Tailwind CSS + i18n EN/JA)
│                   #   Pages: Agents, Soul/Identity, Skills, Memory, Sessions,
│                   #   Co-Agents, Trusted Users, Analytics, Workspace browser
├── dashboard/      # Dioxus dashboard (Rust/WASM fullstack)
├── config/         # Configuration files (hot-reloaded)
├── docs/           # Design docs and assets
└── skills/         # Standard skill definitions (Markdown)
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

### 3. Development (recommended)

```bash
./dev.sh start     # Build + start backend & frontend → http://localhost:3000
./dev.sh stop      # Stop all
./dev.sh restart   # Rebuild & restart backend only (frontend stays)
./dev.sh status    # Show running processes
./dev.sh logs      # Tail server log
```

### 4. Manual startup

```bash
# Backend (with Discord support)
cargo run -p opencrab-server --features discord
# Listening on 0.0.0.0:8080

# Frontend dev server (separate terminal)
cd web && ./dev.sh
# Proxies /api to :8080 → http://localhost:3000
```

### 5. CLI

```bash
cargo run -p opencrab-cli
```

## API Endpoints

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Health check |
| **Agents** | | |
| GET / POST | `/api/agents` | List / create agents |
| GET / DELETE | `/api/agents/{id}` | Get / delete agent |
| **Soul & Identity** | | |
| GET / PUT | `/api/agents/{id}/soul` | Get / update soul (Big Five traits) |
| GET / PUT | `/api/agents/{id}/identity` | Get / update identity |
| GET / POST | `/api/agents/{id}/soul/presets` | List / save soul presets |
| DELETE | `/api/agents/{id}/soul/presets/{preset_id}` | Delete a soul preset |
| POST | `/api/agents/{id}/soul/presets/{preset_id}/apply` | Apply a soul preset |
| **Skills** | | |
| GET / POST | `/api/agents/{id}/skills` | List / add skills |
| POST | `/api/agents/{id}/skills/{skill_id}/toggle` | Toggle skill on/off |
| **Memory** | | |
| GET | `/api/agents/{id}/memory/curated` | List curated memories |
| POST | `/api/agents/{id}/memory/search` | Search memory (FTS5) |
| GET / POST | `/api/agents/{id}/memory/index` | Get index status / trigger index build |
| **Sessions** | | |
| GET / POST | `/api/sessions` | List / create sessions |
| GET | `/api/sessions/{id}` | Get session detail |
| POST | `/api/sessions/{id}/messages` | Send message |
| GET | `/api/sessions/{id}/logs` | Get session logs |
| POST | `/api/sessions/{id}/mentor` | Send mentor instruction |
| **Analytics** | | |
| GET | `/api/agents/{id}/analytics` | Get agent analytics summary |
| GET | `/api/agents/{id}/analytics/detail` | Get detailed analytics |
| **Workspace** | | |
| GET | `/api/agents/{id}/workspace` | List workspace files |
| GET / PUT | `/api/agents/{id}/workspace/{*path}` | Read / write file |
| **Discord** | | |
| GET / PUT / DELETE | `/api/agents/{id}/discord` | Get / save / remove Discord bot config |
| POST | `/api/agents/{id}/discord/start` | Start Discord gateway |
| POST | `/api/agents/{id}/discord/stop` | Stop Discord gateway |
| **Co-Agents** | | |
| GET / POST | `/api/agents/{id}/co-agents` | List / register co-agents |
| PATCH / DELETE | `/api/agents/{id}/co-agents/{co_agent_id}` | Update / remove co-agent |
| **Trusted Users** | | |
| GET / POST | `/api/agents/{id}/trusted-users` | List / add trusted users |
| PATCH / DELETE | `/api/agents/{id}/trusted-users/{user_id}` | Update / remove trusted user |

## Action System

The ActionDispatcher registers **27 actions** across 7 categories, invokable by agents during conversations:

| Category | Actions | Description |
|----------|---------|-------------|
| **Common** (6) | `send_speech`, `send_noreact`, `generate_inner_voice`, `update_impression`, `declare_done`, `get_system_info` | Core communication and session control |
| **Workspace** (6) | `ws_read`, `ws_write`, `ws_edit`, `ws_list`, `ws_delete`, `ws_mkdir` | Sandboxed per-agent file operations |
| **Learning** (3) | `learn_from_experience`, `learn_from_peer`, `reflect_and_learn` | Self-improvement through experience and reflection |
| **Search & Memory** (5) | `search_my_history`, `summarize_and_save`, `create_my_skill`, `browse_memory_index`, `retrieve_memory_nodes` | Memory search, curation, and Agentic RAG |
| **LLM** (5) | `select_llm`, `evaluate_response`, `analyze_llm_usage`, `recall_model_experiences`, `save_model_insight` | Dynamic LLM selection, evaluation, and meta-analysis |

A **bridge executor** connects the ActionDispatcher to the SkillEngine, allowing skill definitions to invoke actions.

## Skills

Standard skills are defined as Markdown files in `skills/`:

| Skill | File | Description |
|-------|------|-------------|
| Autonomous Mode | `autonomous.skill.md` | Self-directed participation without a facilitator |
| Self-Learning | `self-learning.skill.md` | Experience-based capability acquisition |
| Workspace Management | `workspace-management.skill.md` | File and directory operations within the agent workspace |
| LLM Selection | `llm-selection.skill.md` | Dynamic model selection based on task requirements |
| LLM Meta-Analysis | `llm-meta-analysis.skill.md` | Cross-model performance analysis and insight extraction |

## Configuration

Configuration is loaded from `config/default.toml` and **hot-reloaded** when files in `config/` change:

- **Agent settings** -- `heartbeat_interval_secs` (default 29), `heartbeat_enabled` (default false), `workspace_path`, `max_workspace_size_mb`
- **LLM providers** -- Default provider, per-use-case model selection, fallback chains, model aliases, self-selection toggle
- **Gateway settings** -- REST port (8080), per-agent Discord token (DB-persisted), CLI toggle
- **Database** -- SQLite path
- **Tools** -- Shell commands with per-command permission levels (`agent` / `owner`)

The `[tools]` section supports config-driven tool definitions with permission levels, hot-reloaded without server restart.

## Testing

```bash
# Run all tests (~171 tests)
cargo test --workspace

# Run tests for a specific crate
cargo test -p opencrab-db       # 52 tests
cargo test -p opencrab-actions  # 45 tests
cargo test -p opencrab-llm      # 34 tests
cargo test -p opencrab-gateway  # 32 tests
cargo test -p opencrab-core     #  8 tests

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
| Frontend | React + Vite + Tailwind CSS |
| Dashboard | Dioxus (fullstack, Rust/WASM) |
| i18n | react-i18next (English / Japanese) |
| Discord | serenity (per-agent gateway) |
| Serialization | serde / serde_json |
| Error Handling | anyhow / thiserror |
| Logging | tracing / tracing-subscriber |
| Config Watch | notify_debouncer_mini |

## License

MIT
