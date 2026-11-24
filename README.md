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

> **初めての人はここから / New here?** Start with [docs/onboarding.md](docs/onboarding.md).

---

**OpenCrab** lets you create, manage, and run AI agents that actually remember what happened. Most agent frameworks discard conversation history once the context window fills up — OpenCrab doesn't.

- **Memory that persists** — Token-budget compaction summarizes old conversations into topic nodes stored in a memory index. Agents can recall past context via Agentic RAG, not just start from scratch.
- **Every conversation saved** — All session logs are automatically recorded to the database. You don't rely on the agent to decide what's worth keeping.
- **Subtask delegation** — Agents spawn async subtasks that run in parallel without blocking the main conversation.
- **Gateways outside the core** — Discord, Nostr, Web and CLI gateways are separate processes (in [kojira/opencrab-gateways](https://github.com/kojira/opencrab-gateways)) that talk to the core over one Unix socket. The core keeps running while a gateway is replaced.

## Features

- **Multi-Provider LLM Support** — OpenAI, Anthropic, Google Gemini, OpenRouter, Ollama, llama.cpp, plus local CLI/agent backends (Codex CLI, Cursor CLI, ChatGPT direct API via `~/.codex/auth.json`, and ACP agents) — with model aliases and a fallback chain
- **Agent Personality System** — Soul/Identity model with personality text and saveable presets
- **Memory Management** — Curated memories, session logs with FTS5 search, and hierarchical memory index with LLM-powered Agentic RAG
- **Conversation Compaction** — Token-budget-based automatic compaction; replaces older messages with memory index topic summaries, keeping recent logs in full
- **Skill System** — Standard and acquired skills with effectiveness tracking, usage metrics, and guidance-based execution where the LLM dynamically calls `execute_shell`
- **External gateways** — Gateways connect to the core's gate socket (`[gate] listen_socket`) and declare their own tools (operations) in `hello`; the core shows those tools to the agent and calls back into the gateway. See [docs/design/external-gate.md](docs/design/external-gate.md)
- **Per-conversation model** — Each conversation has its own model, set by the owner's `/model` command in a gateway or by the agent's `select_llm`; it applies from the next turn
- **Co-Agent / caller levels** — Every turn carries a caller level (`owner` / `co_agent` / `trusted_user` / `agent`) decided by the gateway; tool visibility depends on it
- **Sandboxed Workspace** — Per-agent file operations with path traversal protection
- **Time Triggers** — Interval (`@every 30m`) and time-of-day (cron) triggers are the same thing: rows in `agent_schedules`, several per session, each with its own `message` (the prompt the agent receives when it fires). One central scheduler fires every row through the same TimedFire path into the session's gateway; triggers that land on the same session run as separate, serialized turns. Changing a row's expression or re-enabling it never fires immediately — the next fire is always after now
- **Self-Learning** — Experience-based learning, peer learning, reflection, and skill creation
- **Response Evaluation** — Quality scoring after each interaction
- **Cost Tracking** — Token usage, latency, and estimated cost per model (`model_pricing`)
- **Hot-Reload Configuration** — `config/` directory watched with `notify_debouncer_mini`; ToolsConfig live-updates without restart
- **Tool Allowed Commands** — Per-agent allowed shell commands (`agent_allowed_commands`), managed from the dashboard, the REST API or the agent's own tools

## Architecture

```
opencrab/
├── crates/
│   ├── core/       # Agent engine, soul, identity, memory, skills, workspace, heartbeat
│   ├── llm/        # Multi-provider LLM abstraction, routing, metrics, pricing
│   ├── llm-types/  # LLM type definitions only (kept separate to keep deps light)
│   ├── gateway/    # GatewayActions trait + shared message types (no platform SDK)
│   ├── actions/    # Action dispatcher, background execution runtime, policy tables
│   ├── db/         # SQLite persistence with FTS5 full-text search
│   ├── mcp/        # MCP client (external tool servers as child processes)
│   ├── server/     # Axum REST API server (hot-reload config watcher), agent response pipeline
│   ├── cli/        # Database-management REPL CLI
│   ├── extgate/    # Core side of the gate socket protocol (hello, said, say, invoke, command)
│   └── gate-client/# Client side of the protocol, used by the gateway processes
├── web/            # React frontend (Vite + Tailwind CSS + i18n EN/JA)
├── config/         # Configuration files (hot-reloaded)
├── docs/           # Design docs and assets
└── skills/         # Standard skill definitions (Markdown)
```

The gateway processes (`discord-gateway`, `nostr-gateway`, `web-gateway`, `cli-gateway`), their
end-to-end QC and the process conformance suite live in
[kojira/opencrab-gateways](https://github.com/kojira/opencrab-gateways); the Bluesky gateway lives in
[kojira/opencrab-bluesky-gateway](https://github.com/kojira/opencrab-bluesky-gateway). They talk to this core only
over the gate socket protocol; the Rust gateways use `gate-client`, pinned to an exact core revision.
Platform settings (bot tokens, Nostr keys, channels, trusted users, owner IDs) belong to each
gateway, not to the core.

**Direction of travel**: the goal is a structure where the **core keeps running while the outer layers (transports, extensions) can be swapped without downtime** — ultimately so that agents can develop opencrab itself. Generic functionality must not live in transport crates, state belongs to the core, and the upper layer should not name individual gateways. See **[docs/design-plugin-architecture.md](docs/design-plugin-architecture.md)** before adding a new gateway or moving code between crates.

## Prerequisites

- [Rust](https://rustup.rs/) 1.98.0 (the version CI pins; edition 2021)
- SQLite (bundled via `rusqlite`)

## Getting Started

### 1. Clone and build

```bash
git clone https://github.com/kojira/opencrab.git
cd opencrab
cargo build
```

### 2. Create the config file

Copy the neutral template. Its default model is `chatgpt:gpt-6-sol` (ChatGPT login in
`~/.codex/auth.json`), which has a context budget in a fresh database, so the server starts as-is.
Other providers are set up from the dashboard (see
[First-time setup](#first-time-setup-recommended) below). To make another model the default,
register its budget first (`PUT /api/llm/model-pricing`, see [docs/onboarding.md](docs/onboarding.md)).

```bash
cp config/default.toml.example config/default.toml
```

> **Security note:** the dashboard has no authentication. Since it handles secrets
> like LLM provider API keys, only expose the dashboard on a trusted
> network (e.g. localhost or behind a VPN/reverse proxy with auth).

### 3. Set environment variables (optional)

OpenCrab talks to LLMs through pluggable providers, chosen by `[llm] default_provider`
in `config/default.toml` or from the dashboard setup wizard. Because providers and their
API keys can be configured from the dashboard and saved to the DB, API keys are optional here.

If you prefer environment variables for LLM providers, set the API keys directly or in `.env`:

```bash
export OPENAI_API_KEY="sk-..."
export ANTHROPIC_API_KEY="sk-ant-..."
# Optional
export GOOGLE_API_KEY="..."
export OPENROUTER_API_KEY="..."
```

#### `.env`

The server and CLI load `.env` at startup, and `config/default.toml` references values as
`${VAR}` — an unset variable expands to an empty string, so a missing `.env` silently
leaves the value empty:

```bash
cp .env.example .env
# then edit .env
```

`OPENCRAB_SUBTASK_AUTO_DISPATCH` (`0`/`false`/`off`/`no`) turns background tool execution
off without editing the TOML (see [Configuration](#configuration)).

Discord, Nostr and other platform settings (bot tokens, owner IDs, Nostr keys and the
master key that encrypts them) are not read by the core; they belong to the gateway
processes. See the gateway repositories for their settings.

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
# Backend
cargo run -p opencrab-server
# REST API on 0.0.0.0:8080 ([gateway.rest] port). If [gate] listen_socket is set,
# it also listens on that Unix socket for gateway processes.

# Frontend dev server (separate terminal)
cd web && ./dev.sh
# Proxies /api to $OPENCRAB_API_URL (default http://localhost:8080) → http://localhost:3000
# e.g. OPENCRAB_API_URL=http://localhost:18080 ./dev.sh when [gateway.rest] port = 18080
```

### 6. CLI

```bash
cargo run -p opencrab-cli --bin opencrab-cli
```

## First-time setup (recommended)

Once the server and dashboard are running, open `http://localhost:3000` and go to
**Setup** (`/setup`). The guided wizard has four steps, each checked from the current state:

1. **LLM provider** — enable a provider and set its API key (saved to the DB, hot-reloaded)
2. **Agent** — create an agent; the standard skills in `skills/*.skill.md` are seeded automatically
3. **Gateway** — start a gateway process from
   [kojira/opencrab-gateways](https://github.com/kojira/opencrab-gateways) (Discord, Nostr, web,
   terminal) or [kojira/opencrab-bluesky-gateway](https://github.com/kojira/opencrab-bluesky-gateway)
   and point it at the core's gate socket. The page shows the socket path, the agent id and the
   terminal gateway commands filled with them; it is done while a gateway is connected.
4. **First conversation** — send a message from the gateway; it is done once the agent replies.

[docs/onboarding.md](docs/onboarding.md) walks the same path in text.

The Home page also shows a **Setup checklist** card that tracks your progress and links
back into the wizard until every step is done.

Standard skills are seeded from `skills/*.skill.md` (resolved relative to the server's
working directory). If you run the server from a different directory, point it at the
skills folder with `OPENCRAB_SKILLS_DIR=/path/to/skills` so step 2 can seed them.

A few settings still require editing `config/default.toml` and restarting: the REST
port, the database path, the gate socket path, and the initial `[tools]` allowed-command list
(per-agent commands can be added at runtime).

## API Endpoints

See [docs/api.md](docs/api.md) for the full API reference.

## Action System

The ActionDispatcher registers these actions, invokable by agents during conversations:

| Category | Actions | Description |
|----------|---------|-------------|
| **Common** | `generate_inner_voice`, `update_impression`, `declare_done`, `get_system_info` | Core session control and self-narration |
| **Workspace** | `ws_read`, `ws_write`, `ws_edit`, `ws_list`, `ws_delete`, `ws_mkdir`, `view_image`, `generate_image` | Sandboxed per-agent file operations; `view_image` shows an image (workspace path or https URL) on the next LLM call; `generate_image` (owner-only, background) creates a PNG under `generated_images/` via the configured ChatGPT subscription provider |
| **Learning** | `learn_from_experience`, `learn_from_peer`, `reflect_and_learn` | Self-improvement through experience and reflection |
| **Skills** | `create_my_skill`, `retire_my_skill`, `restore_my_skill`, `read_skill` | Self-created skill lifecycle |
| **Search & Memory** | `search_my_history`, `summarize_and_save`, `browse_memory_index`, `retrieve_memory_nodes`, `search_memory_index` | Memory search, curation, and Agentic RAG |
| **Memory tags** | `tag_topic`, `untag_topic`, `merge_tags` | Trusted-only tagging of memory topics (many-to-many) |
| **Memory units** | `survey_my_history`, `read_my_history`, `record_memory_unit`, `retract_memory_unit`, `plan_next_memory_window` | Trusted-only: survey/read own raw logs, declare bounded ranges as memory units, and choose where the next declare window starts and how wide it is |
| **Memory condense** | `record_memory_core`, `update_memory_core`, `retract_memory_core` | Trusted-only: distill own memory units into personality-core principles (`node_type='meta'`) that link back to the source units |
| **LLM** | `select_llm`, `evaluate_response`, `analyze_llm_usage`, `recall_model_experiences`, `save_model_insight` | Dynamic LLM selection, evaluation, and meta-analysis |
| **Soul** | `update_instructions` | Owner-only agent behavioral instruction update |
| **Task ledger** | `open_task`, `update_task_contract`, `record_task_progress`, `close_task`, `get_task` | Long-running task bookkeeping |

One further action, `execute_shell` (run shell commands from the agent's allowed command
list), is **config-driven**: it is registered from `[tools.shell]` rather than by
`ActionDispatcher::new()`, so it is absent when shell tooling is disabled.

The rows above name every action in `ActionDispatcher::new().action_names()`. Drift is caught
by a test rather than by review — `readme_action_table_matches_the_dispatcher`
(`crates/actions/src/dispatcher.rs`) parses this table and requires it to equal the registered
set in both directions. Absolute counts are deliberately not written here: they went stale
twice (#203), and the table itself is the check.

In addition, **gateway actions** are invokable via natural language. They come from two places:

1. **The core's own tools** (`SystemGatewayActions`, `crates/server/src/system_actions/`). These exist on every turn, whatever gateway the turn came from.
2. **Tools a gateway declares** in its `hello` frame (`operations`, e.g. Discord's `reply` / `reaction`). The core shows them to the agent only on turns of that gateway's conversations and invokes them back over the gate socket (`crates/extgate/src/ops_projection.rs`). Their names, schemas and caller levels are owned by the gateway, so they are not listed here; see [docs/design/external-gate.md](docs/design/external-gate.md) for the declaration format.

| Category | Actions | Available on |
|----------|---------|--------------|
| **Config** | `configure_llm_provider`, `configure_self`, `configure_mcp_server` | all turns (`crates/server/src/system_actions/`) |
| **Memory** | `rebuild_memory_index`, `update_memory_index_config` | all turns (`crates/server`) |
| **Tool Permissions** | `add_allowed_command`, `list_allowed_commands`, `remove_allowed_command`, `manage_allowed_commands` | all turns (`crates/server/src/agent_management.rs`, `system_actions/`) |
| **Subtask** | `spawn_subtask`, `cancel_subtask`, `steer_subtask`, `report_progress` | all turns (`crates/server`) |
| **Schedules (#455, #477, #612)** | `get_my_schedules`, `set_my_schedule`, `update_my_schedule`, `delete_my_schedule`, `run_my_schedule` | all turns (`crates/server/src/agent_schedule.rs`) — an agent registers/reads/updates/deletes its own cron / `@every` triggers for the current session (each fires its own message into the session on the central scheduler); update/delete/run take an id from `get_my_schedules` and only touch rows that belong to the caller's own agent id and current session; `run_my_schedule` (owner / co_agent only) fires a row now without waiting, through the exact same path as a timed fire (`heartbeat_fire::run_one_heartbeat`), and does not update `last_fired_at` |
| **Skills** | `create_skill` | all turns (`crates/server/src/agent_management.rs`) |

The rows above name every action in `SystemGatewayActions::own_definitions()`. The test that
compares them with this table, `server_gateway_action_table_matches_own_definitions`
(`crates/server/src/system_actions/tests/definitions.rs`), is currently compiled out
(`#[cfg(any())]`), so this table is checked by review only.

MCP servers configured per agent add tools named `mcp__<server>__<tool>`.

Visibility is not uniform. The single table is `crates/actions/src/bridge/policy.rs`
(`OWNER_ONLY_ACTIONS` / `TRUSTED_ONLY_ACTIONS`), consulted by both tool listing and execution.

- **Owner only** (`OWNER_ONLY_ACTIONS`): `update_instructions`, `configure_llm_provider`,
  `manage_allowed_commands`, `configure_self`, `configure_mcp_server`, `execute_shell`, the
  `ws_*` file tools, `add_allowed_command`, `remove_allowed_command`, `run_my_schedule` and
  `generate_image`. A `co_agent` caller is treated as owner (`CallerIdentity::is_owner_equivalent`).
- **Trusted only** (`TRUSTED_ONLY_ACTIONS`): skill creation and learning, the schedule CRUD tools,
  `update_memory_index_config`, `get_system_info`, `list_allowed_commands`, the memory tag tools
  and the memory unit / memory core tools. These are neither listed nor executable on a turn driven
  by an untrusted external user (`agent` caller).

Tools a gateway declares carry their own `authorization.allowed_callers` in the declaration.

`create_skill` (a gateway action, `source_type="acquired"`) and `create_my_skill` (a core
action, `source_type="self_created"`, `situation_pattern` required) are deliberately kept as
two separate tools: #157 only moves generic implementations out of the transport layer, and
dropping a tool name would break calls recorded in past conversation logs.

## Model selection

Each conversation (session) has its own model (`sessions.model`); a turn reads it once at start
and runs on that one model. It is changed in two ways, both through
`ModelAdministration::set_session_model`:

- the owner's `/model` command in a gateway (the gate socket `command` frame with
  `set_session_model`), and
- the agent's `select_llm` tool.

The change applies from the conversation's next turn and lasts until changed again. The
running turn keeps its model. A name that is not in the `/model` list (`GET /api/llm/model-choices`)
is rejected and the list is returned. `select_llm` works only in a conversation, not inside a
subtask.

`get_system_info` returns `current_turn_model` (this turn), `conversation_model` (the next
turn) and `selectable_models` (the `/model` list).

When an LLM call fails and ends the turn, the core records a `turn_failed` event with the model
and the provider error; it appears in the conversation history on the next turn.

## Skills

Skills have no executable type — each skill defines a `guidance` field describing how to use it, and the LLM dynamically executes skills via actions at runtime.

`SkillSource` has two variants:
- **Standard** — Loaded from Markdown files in `skills/`
- **Acquired** — Created through learning and experience

Standard skills defined in `skills/`:

| Skill | File | Description |
|-------|------|-------------|
| Autonomous Mode | `autonomous.skill.md` | Self-directed participation without a facilitator |
| OpenCrab Handbook | `opencrab-handbook.skill.md` | Map of OpenCrab itself: caller levels, how settings are changed, where to find current facts |
| Self-Learning | `self-learning.skill.md` | Experience-based capability acquisition |
| Workspace Management | `workspace-management.skill.md` | File and directory operations within the agent workspace |
| LLM Selection | `llm-selection.skill.md` | When and how to change the conversation's model with `select_llm` |
| LLM Meta-Analysis | `llm-meta-analysis.skill.md` | Cross-model performance analysis and insight extraction |
| Cursor CLI | `cursor-cli-local.skill.md` | Driving the Cursor CLI (`agent`) through `execute_shell`. Requires granting the command per agent via `agent_allowed_commands`; it is intentionally not in the global allowlist |

### Skills usable by anyone (owner designation)

Some tools are owner-only or trusted-only (see above). A skill can carry a list of tools
(`skills.tools`). When the owner turns on **Usable by anyone (owner designation)** for a skill
(`skills.agent_visible`, set only from the dashboard or `PUT /api/agents/{id}/skills/{skill_id}`),
a turn that opens that skill with `read_skill` may use the listed tools for the rest of the run,
whoever the caller is. The `read_skill` result names those tools (`unlocked_tools`), and they appear
in the tool list from then on. Nothing else is narrowed or widened: depth, sub-engine and run
allowlist rules still apply, and a skill that is not designated, or a tool that the skill does not
list, grants nothing. The one rule lives in `crates/actions/src/bridge/gate.rs`
(`SkillRow::granted_tools` decides what a skill grants).

- The owner can also designate by asking the agent in conversation: in an owner-equivalent turn
  (owner / co_agent), `create_my_skill` takes `anyone_can_use: true` and `actions` (the tools) for a
  new or existing skill. In any other turn, setting `anyone_can_use`, or changing the tools of a
  designated skill, is refused with a message that the owner must ask for it
  (`SkillRow::set_access` is the one rule, also used by the REST update).
- If a non-owner turn rewrites a designated skill (`create_my_skill` / `create_skill` on an existing
  name), the designation is removed and the tool result says so; the owner designates it again.
- Skills that existed before schema v66 start with no tools, so they grant nothing until the owner
  sets the tools.


## Configuration

Configuration is loaded from `config/default.toml` and **hot-reloaded** when files in `config/` change:

- **Agent settings** — `workspace_path`, `max_workspace_size_mb`. The legacy `heartbeat_interval_secs`, `heartbeat_enabled` and `heartbeat_min_interval_secs` keys are still accepted but no longer read: time triggers live in `agent_schedules` (#612)
- **LLM providers** — Default provider and default model (`[llm] default_provider` / `default_model`), providers (`[llm.providers.*]`), fallback chain (`[llm.fallback]`), model aliases (`[llm.aliases]`). Provider settings are also editable from the dashboard and saved to the DB, which then takes precedence over this file
- **REST and gate socket** — REST port (`[gateway.rest] port`, 8080) and the gate socket path for gateway processes (`[gate] listen_socket`; empty = do not listen)
- **Database** — SQLite path
- **Tools** — Shell commands with per-command permission levels (`agent` / `owner`)
- **Background tool execution** — `[subtask] auto_dispatch` (default `true`). Tool calls run in the background so the response loop never blocks; results are re-injected into the conversation when they finish. Set it to `false` (or export `OPENCRAB_SUBTASK_AUTO_DISPATCH=0`, which takes precedence) to fall back to fully synchronous tool execution. See "非ブロックツール実行" in `docs/DESIGN.md` for which tools stay inline and why.

The `[tools]` section supports config-driven tool definitions with permission levels, hot-reloaded without server restart.

## Testing

```bash
# Run all tests (this is what CI runs).
cargo test --workspace --all-features

# Run tests for a single crate (see [workspace] members in Cargo.toml for the full list)
cargo test -p opencrab-db
cargo test -p opencrab-actions
cargo test -p opencrab-llm
cargo test -p opencrab-core
cargo test -p opencrab-extgate

# Run the in-process E2E API tests (builds the real router; no server needed)
cargo test -p opencrab-server --test api_e2e

# Local-only E2E harness (NOT for CI). Drives a running server over HTTP/SSE with a
# real LLM to verify tool dispatch / cancellation end-to-end. Gated by OPENCRAB_E2E=1.
# Prereqs: local server running (./dev.sh restart) + ~/.codex/auth.json authenticated.
cp .env.example .env   # adjust values if needed (no secrets committed)
# OPENCRAB_E2E_OWNER_ID is also required (no default): the user_id used for owner
# authorization checks. Set it in .env; tests skip when it is unset.
OPENCRAB_E2E=1 cargo test -p opencrab-server --test e2e_local -- --ignored --nocapture
```

The authoritative test counts are the summary lines `cargo test --workspace --all-features` prints
itself. No totals are reproduced here — neither workspace-wide nor per-crate — because any number
written into this file goes stale on the next PR that adds a test, and nothing in CI verifies it.
Some tests are reported as `ignored`: those need something the suite cannot provide on its own — a
running server (`OPENCRAB_E2E=1`, see above), real provider credentials — plus a couple of doc
examples.

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
| Gateways | separate processes over a Unix socket ([opencrab-gateways](https://github.com/kojira/opencrab-gateways)) |
| Serialization | serde / serde_json |
| Error Handling | anyhow / thiserror |
| Logging | tracing / tracing-subscriber |
| Config Watch | notify_debouncer_mini |

## License

MIT
