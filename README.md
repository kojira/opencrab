<p align="center">
  <img src="docs/icon.png" alt="OpenCrab" width="128" />
</p>

<h1 align="center">OpenCrab</h1>

<p align="center">
  An autonomous AI agent framework built in Rust.
</p>

<p align="center">
  <a href="https://github.com/kojira/opencrab/actions/workflows/ci.yml">
    <img src="https://github.com/kojira/opencrab/actions/workflows/ci.yml/badge.svg" alt="CI" />
  </a>
</p>

---

**OpenCrab** lets you create, manage, and run AI agents that actually remember what happened. Most agent frameworks discard conversation history once the context window fills up — OpenCrab doesn't.

- **Memory that persists** — Token-budget compaction summarizes old conversations into topic nodes stored in a memory index. Agents can recall past context via Agentic RAG, not just start from scratch.
- **Every conversation saved** — All session logs are automatically recorded to the database. You don't rely on the agent to decide what's worth keeping.
- **Subtask delegation** — Agents spawn async subtasks that run in parallel without blocking the main conversation.
- **Discord-native** — Each agent gets its own Discord gateway with independent lifecycle. Agents coexist naturally in channels, including Bot-to-Bot communication.

## Features

- **Multi-Provider LLM Support** — OpenAI, Anthropic, Google Gemini, OpenRouter, Ollama, llama.cpp with intelligent routing and automatic fallback
- **Agent Personality System** — Soul/Identity model with personality text and saveable presets
- **Memory Management** — Curated memories, session logs with FTS5 search, and hierarchical memory index with LLM-powered Agentic RAG
- **Conversation Compaction** — Token-budget-based automatic compaction; replaces older messages with memory index topic summaries, keeping recent logs in full
- **Skill System** — Standard and acquired skills with effectiveness tracking, usage metrics, and guidance-based execution where the LLM dynamically calls `execute_shell`
- **Multi-Channel Communication** — REST API, CLI, WebSocket, and Discord gateway adapters
- **Per-Agent Discord Gateway** — DB-persisted Discord config per agent with independent start/stop lifecycle management
- **Message Debounce** — Per (channel, sender) debounce window batches rapid messages into a single request
- **Co-Agent Management** — Trust relationships between agents with configurable permission levels (owner/agent/co-agent)
- **Trusted User Whitelist** — Per-agent Discord user trust management
- **Sandboxed Workspace** — Per-agent file operations with path traversal protection
- **Heartbeat Loop** — Per-channel periodic autonomous agent activity with configurable interval and graceful shutdown
- **Self-Learning** — Experience-based learning, peer learning, reflection, and skill creation
- **LLM Self-Selection** — Agents dynamically select LLMs per task based on past experience
- **Response Evaluation** — Quality scoring after each interaction
- **Cost Tracking** — Token usage, latency, and estimated cost per model
- **Mentor Instruction (planned)** — Owner registers behavioral rules for specific scenarios; agents reference them for case-based decision-making
- **Hot-Reload Configuration** — `config/` directory watched with `notify_debouncer_mini`; ToolsConfig live-updates without restart
- **Channel Whitelist** — Per-channel readable/writable/whitelist management via `discord_channel_config` table
- **Tool Allowed Commands** — Agents manage their own tool permission lists via gateway actions

## Architecture

```
opencrab/
├── crates/
│   ├── core/       # Agent engine, soul, identity, memory, skills, workspace, heartbeat
│   ├── llm/        # Multi-provider LLM abstraction, routing, metrics, pricing
│   ├── gateway/    # Multi-channel message gateway (REST, CLI, WebSocket, Discord)
│   ├── actions/    # Action dispatcher (28 actions) and skill bridge executor
│   ├── db/         # SQLite persistence with FTS5 full-text search
│   ├── server/     # Axum REST API server with hot-reload config watcher
│   ├── cli/        # Interactive REPL CLI
│   └── discord/    # Discord gateway with per-agent manager and message loop
├── web/            # React frontend (Vite + Tailwind CSS + i18n EN/JA)
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

By default, OpenCrab uses [hermit-shell](https://github.com/kojira/hermit-shell) (`localhost:8765`) as the LLM backend. hermit-shell is a macOS-native OpenAI-compatible proxy for Anthropic that retrieves API keys from the macOS Keychain automatically.

If not using hermit-shell, set provider API keys directly:

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

See [docs/api.md](docs/api.md) for the full API reference.

## Action System

The ActionDispatcher registers **28 actions** across 7 categories, invokable by agents during conversations:

| Category | Actions | Description |
|----------|---------|-------------|
| **Common** (7) | `send_speech`, `send_noreact`, `no_reply`, `generate_inner_voice`, `update_impression`, `declare_done`, `get_system_info` | Core communication and session control |
| **Workspace** (6) | `ws_read`, `ws_write`, `ws_edit`, `ws_list`, `ws_delete`, `ws_mkdir` | Sandboxed per-agent file operations |
| **Learning** (3) | `learn_from_experience`, `learn_from_peer`, `reflect_and_learn` | Self-improvement through experience and reflection |
| **Search & Memory** (5) | `search_my_history`, `summarize_and_save`, `create_my_skill`, `browse_memory_index`, `retrieve_memory_nodes` | Memory search, curation, and Agentic RAG |
| **LLM** (5) | `select_llm`, `evaluate_response`, `analyze_llm_usage`, `recall_model_experiences`, `save_model_insight` | Dynamic LLM selection, evaluation, and meta-analysis |
| **Soul** (1) | `update_instructions` | Owner-only agent behavioral instruction update |
| **Shell** (1) | `execute_shell` | Run shell commands from the agent's allowed command list |

In addition, the Discord gateway supports **gateway-only actions** invokable via natural language:

| Category | Actions |
|----------|---------|
| **Discord** | `discord_list_guilds`, `discord_list_channels`, `discord_channel_config`, `discord_add_reaction`, `discord_send_file` |
| **Skills** | `create_skill` |
| **Memory** | `rebuild_memory_index`, `update_memory_index_config` |
| **Tool Permissions** | `add_allowed_command`, `list_allowed_commands`, `remove_allowed_command` |
| **Subtask** | `spawn_subtask`, `cancel_subtask`, `report_progress` |

## Skills

Skills have no executable type — each skill defines a `guidance` field describing how to use it, and the LLM dynamically executes skills via actions at runtime.

`SkillSource` has two variants:
- **Standard** — Loaded from Markdown files in `skills/`
- **Acquired** — Created through learning and experience

Standard skills defined in `skills/`:

| Skill | File | Description |
|-------|------|-------------|
| Autonomous Mode | `autonomous.skill.md` | Self-directed participation without a facilitator |
| Self-Learning | `self-learning.skill.md` | Experience-based capability acquisition |
| Workspace Management | `workspace-management.skill.md` | File and directory operations within the agent workspace |
| LLM Selection | `llm-selection.skill.md` | Dynamic model selection based on task requirements |
| LLM Meta-Analysis | `llm-meta-analysis.skill.md` | Cross-model performance analysis and insight extraction |

## Configuration

Configuration is loaded from `config/default.toml` and **hot-reloaded** when files in `config/` change:

- **Agent settings** — `heartbeat_interval_secs` (default 1800), `heartbeat_enabled` (default false), `workspace_path`, `max_workspace_size_mb`
- **LLM providers** — Default provider (hermit-shell proxy at localhost:8765 by default), per-use-case model selection, fallback chains, model aliases, self-selection toggle
- **Gateway settings** — REST port (8080), per-agent Discord token (DB-persisted), CLI toggle
- **Database** — SQLite path
- **Tools** — Shell commands with per-command permission levels (`agent` / `owner`)

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
| i18n | react-i18next (English / Japanese) |
| Discord | serenity (per-agent gateway) |
| Serialization | serde / serde_json |
| Error Handling | anyhow / thiserror |
| Logging | tracing / tracing-subscriber |
| Config Watch | notify_debouncer_mini |

## License

MIT
