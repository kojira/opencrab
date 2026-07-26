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
│   ├── llm-types/  # LLM type definitions only (kept separate to keep deps light)
│   ├── gateway/    # Gateway traits and adapters (transport-agnostic contracts)
│   ├── actions/    # Action dispatcher, background execution runtime, policy tables
│   ├── db/         # SQLite persistence with FTS5 full-text search
│   ├── mcp/        # MCP client (external tool servers as child processes)
│   ├── server/     # Axum REST API server, agent response pipeline, web gateway
│   ├── cli/        # Interactive REPL CLI
│   ├── discord/    # Discord gateway with per-agent manager and message loop
│   ├── nostr/      # Nostr gateway (per-session queue, concurrency cap)
│   └── voice/      # Voice session support
├── web/            # React frontend (Vite + Tailwind CSS + i18n EN/JA)
├── config/         # Configuration files (hot-reloaded)
├── docs/           # Design docs and assets
└── skills/         # Standard skill definitions (Markdown)
```

**Direction of travel**: the goal is a structure where the **core keeps running while the outer layers (transports, extensions) can be swapped without downtime** — ultimately so that agents can develop opencrab itself. Generic functionality must not live in transport crates, state belongs to the core, and the upper layer should not name individual gateways. See **[docs/design-plugin-architecture.md](docs/design-plugin-architecture.md)** before adding a new gateway or moving code between crates.

Some of this is not yet true (the web gateway currently lives inside `server`, and `discord` still holds generic tools). Those are tracked as issues and are being unwound.

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

### 2. Create the config file

Copy the neutral template. You do **not** need to hand-edit it — first-time setup
is done entirely from the dashboard (see [First-time setup](#first-time-setup-recommended) below).

```bash
cp config/default.toml.example config/default.toml
```

> **Security note:** the dashboard has no authentication. Since first-time setup
> handles secrets like Discord bot tokens, only expose the dashboard on a trusted
> network (e.g. localhost or behind a VPN/reverse proxy with auth).

### 3. Set environment variables (required for Discord, optional otherwise)

By default, OpenCrab uses [hermit-shell](https://github.com/kojira/hermit-shell) (`localhost:8765`) as the LLM backend. hermit-shell is a macOS-native OpenAI-compatible proxy for Anthropic that retrieves API keys from the macOS Keychain automatically.

LLM providers can be configured from the dashboard, so API keys are optional here.
Discord is different: `config/default.toml` no longer holds an owner ID of its own —
it reads `${OWNER_DISCORD_ID}` from the environment — so the Discord gateway needs a
`.env` file (or exported variables) to know who the owner is.

If you prefer environment variables for LLM providers too, set the API keys directly:

```bash
export OPENAI_API_KEY="sk-..."
export ANTHROPIC_API_KEY="sk-ant-..."
# Optional
export GOOGLE_API_KEY="..."
export OPENROUTER_API_KEY="..."
export DISCORD_TOKEN="..."
```

#### `.env` (how per-environment values are supplied)

Values that differ per environment (Discord bot token, your Discord user ID) are kept
in a `.env` file rather than in the tracked config. The server and CLI load `.env` at
startup, and `config/default.toml` references the values as `${VAR}` — an unset variable
expands to an empty string, so a missing `.env` silently leaves the value empty:

```bash
cp .env.example .env
# then edit .env
```

`OWNER_DISCORD_ID` is your own Discord user ID and gates owner-only behavior. **Set it
whenever you use Discord** (shared gateway or per-agent bots): when it is empty, nobody
is recognized as owner, so owner-only features stop working, DMs from any user are
accepted for agents that have no trusted users registered, and owner-only UI
(forms/modals/buttons) skips its operator check. The server logs a warning whenever a
Discord gateway starts without an owner — at boot for the shared gateway, and for
per-agent bots both at boot (restored from the database) and when a config is saved
from the dashboard. See
[docs/discord.md](docs/discord.md) for details.

### 4. Development (recommended)

```bash
./dev.sh start     # Build + start backend & frontend → http://localhost:3000
./dev.sh stop      # Stop all
./dev.sh restart   # Rebuild & restart backend only (frontend stays)
./dev.sh status    # Show running processes
./dev.sh logs      # Tail server log
```

### 5. Manual startup

```bash
# Backend (with Discord support)
cargo run -p opencrab-server --features discord
# Listening on 0.0.0.0:8080

# Frontend dev server (separate terminal)
cd web && ./dev.sh
# Proxies /api to :8080 → http://localhost:3000
```

### 6. CLI

```bash
cargo run -p opencrab-cli
```

## First-time setup (recommended)

Once the server and dashboard are running, open `http://localhost:3000` and go to
**Setup** (`/setup`). The guided wizard walks you through everything needed to get a
working agent — no `config/default.toml` editing required:

1. **LLM provider** — enable a provider and set its API key (saved to the DB, hot-reloaded)
2. **Agent** — create an agent; the standard skills in `skills/*.skill.md` are seeded automatically
3. **Discord** — set a per-agent bot token; the gateway starts immediately (no restart)
4. **Channel** — whitelist the channel where the agent should respond

The Home page also shows a **Setup checklist** card that tracks your progress and links
back into the wizard until every step is done.

Standard skills are seeded from `skills/*.skill.md` (resolved relative to the server's
working directory). If you run the server from a different directory, point it at the
skills folder with `OPENCRAB_SKILLS_DIR=/path/to/skills` so step 2 can seed them.

A few settings still require editing `config/default.toml` and restarting: the REST
port, the database path, and the initial `[tools]` allowed-command list (agents can add
their own commands at runtime via gateway actions).

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
- **Background tool execution** — `[subtask] auto_dispatch` (default `true`). Tool calls run in the background so the response loop never blocks; results are re-injected into the conversation when they finish. Set it to `false` (or export `OPENCRAB_SUBTASK_AUTO_DISPATCH=0`, which takes precedence) to fall back to fully synchronous tool execution. See "非ブロックツール実行" in `docs/DESIGN.md` for which tools stay inline and why.

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

# Local-only E2E harness (NOT for CI). Drives a running server over HTTP/SSE with a
# real LLM to verify tool dispatch / cancellation end-to-end. Gated by OPENCRAB_E2E=1.
# Prereqs: local server running (./dev.sh restart) + ~/.codex/auth.json authenticated.
cp .env.example .env   # adjust values if needed (no secrets committed)
# OPENCRAB_E2E_OWNER_ID is also required (no default): the user_id used for owner
# authorization checks. Set it in .env; tests skip when it is unset.
OPENCRAB_E2E=1 cargo test -p opencrab-server --test e2e_local -- --ignored --nocapture
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
