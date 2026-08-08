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

- **Multi-Provider LLM Support** — OpenAI, Anthropic, Google Gemini, OpenRouter, Ollama, llama.cpp, plus local CLI/agent backends (Codex CLI, Cursor CLI, ChatGPT direct API via `~/.codex/auth.json`, and ACP agents) — with per-use-case routing and automatic fallback
- **Agent Personality System** — Soul/Identity model with personality text and saveable presets
- **Memory Management** — Curated memories, session logs with FTS5 search, and hierarchical memory index with LLM-powered Agentic RAG
- **Conversation Compaction** — Token-budget-based automatic compaction; replaces older messages with memory index topic summaries, keeping recent logs in full
- **Skill System** — Standard and acquired skills with effectiveness tracking, usage metrics, and guidance-based execution where the LLM dynamically calls `execute_shell`
- **Multi-Channel Communication** — REST API, CLI, WebSocket, web dashboard, Discord and Nostr gateway adapters
- **Per-Agent Discord Gateway** — DB-persisted Discord config per agent with independent start/stop lifecycle management
- **Per-Agent Nostr Gateway** — DB-persisted key and relay config per agent; an agent can generate a key and adopt it as its own identity, which also brings the gateway up (see [Nostr](#nostr))
- **Message Debounce** — Per (channel, sender) debounce window batches rapid messages into a single request
- **Co-Agent Management** — Trust relationships between agents with configurable permission levels (owner/agent/co-agent)
- **Trusted User Whitelist** — Per-agent Discord user trust management
- **Sandboxed Workspace** — Per-agent file operations with path traversal protection
- **Heartbeat Loop** — Periodic autonomous agent activity with configurable interval and graceful shutdown. Firing is **per agent**: an agent that opted in gets its own tick (so an agent with no Discord channel — a Nostr-only one, for example — still runs), and agents that have not opted in keep the older per-channel firing. The speech outlet is transport-independent, so what a tick produces is delivered by whichever gateway the agent has
- **Self-Configured Heartbeat** — Agents read and write their own heartbeat settings (`get_my_heartbeat` / `set_my_heartbeat`); intervals below the configured floor are rejected in the same turn so the agent can retry with a valid value. The stored settings are re-read on every tick, so an agent that already has a heartbeat loop running switches to per-agent firing immediately; what is fixed at startup is only the *set of agents a loop is started for*, so an agent with no loop yet (a Nostr-only one, with no entry under `gateway.discord.agent_ids`) begins firing after the next restart
- **Attachment Anchors** — Discord attachments leave a trace in the stored conversation, not just in the model call: images are passed as vision content parts *and* noted in the message body as `[画像添付: name (type)]`, non-image files as `[添付ファイル: name (type), NB]`. Without the anchor an image left no record in `session_logs`, and later turns concluded the agent had made it up
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
│   ├── gateway/    # GatewayActions trait + shared message types + DiscordGateway (serenity)
│   ├── actions/    # Action dispatcher, background execution runtime, policy tables
│   ├── db/         # SQLite persistence with FTS5 full-text search
│   ├── mcp/        # MCP client (external tool servers as child processes)
│   ├── server/     # Axum REST API server (hot-reload config watcher), agent response pipeline
│   ├── cli/        # Interactive REPL CLI
│   ├── discord/    # Discord gateway with per-agent manager and message loop
│   ├── nostr/      # Nostr gateway: per-agent key/relay config, per-session queue,
│   │               #   concurrency cap, and a thin `nostaro` CLI passthrough
│   ├── web-gateway/# Dashboard gateway: axum router/handlers (POST web/send, SSE GET web/stream),
│   │               #   per-session SSE fan-out, web-{agent}-{conversation} session-id convention,
│   │               #   subtask-completion sink, per-session-serialized response entry point.
│   │               #   Runs agents / persists via the WebAgentRunner trait (implemented by server)
│   └── voice/      # STT/TTS provider layer (OpenAI-compatible STT, VOICEVOX/OpenAI TTS)
├── web/            # React frontend (Vite + Tailwind CSS + i18n EN/JA)
├── config/         # Configuration files (hot-reloaded)
├── docs/           # Design docs and assets
└── skills/         # Standard skill definitions (Markdown)
```

**Direction of travel**: the goal is a structure where the **core keeps running while the outer layers (transports, extensions) can be swapped without downtime** — ultimately so that agents can develop opencrab itself. Generic functionality must not live in transport crates, state belongs to the core, and the upper layer should not name individual gateways. See **[docs/design-plugin-architecture.md](docs/design-plugin-architecture.md)** before adding a new gateway or moving code between crates.

Some of this is not yet true and is tracked as issues: `discord` still holds generic tools, a concrete Discord transport sits in `gateway/` next to the traits, and the `Gateway` trait itself has no consumers yet. These are being unwound.

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

OpenCrab talks to LLMs through pluggable providers, chosen by `[llm] default_provider`
in `config/default.toml` or from the dashboard setup wizard. Because providers and their
API keys can be configured from the dashboard and saved to the DB, API keys are optional here.
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

The ActionDispatcher registers these actions, invokable by agents during conversations:

| Category | Actions | Description |
|----------|---------|-------------|
| **Common** | `generate_inner_voice`, `update_impression`, `declare_done`, `get_system_info` | Core session control and self-narration |
| **Workspace** | `ws_read`, `ws_write`, `ws_edit`, `ws_list`, `ws_delete`, `ws_mkdir` | Sandboxed per-agent file operations |
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

In addition, **gateway actions** are invokable via natural language. Where they are
implemented decides where they are usable: the transport-independent source
(`SystemGatewayActions`, `crates/server/src/system_actions.rs`) is composed into **every**
turn — Discord, web, Nostr, REST and heartbeat alike — while a transport crate's actions
only exist on that transport's turns.

| Category | Actions | Available on |
|----------|---------|--------------|
| **Config** | `configure_llm_provider`, `configure_self`, `configure_nostr`, `configure_mcp_server` | all turns (`crates/server/src/system_actions.rs`) |
| **Memory** | `rebuild_memory_index`, `update_memory_index_config` | all turns (`crates/server`) |
| **Tool Permissions** | `add_allowed_command`, `list_allowed_commands`, `remove_allowed_command`, `manage_allowed_commands` | all turns (`crates/server/src/agent_management.rs`, `system_actions.rs`) |
| **Subtask** | `spawn_subtask`, `cancel_subtask`, `report_progress` | all turns (`crates/server`) |
| **Heartbeat** | `update_heartbeat_instructions`, `read_heartbeat_instructions`, `get_my_heartbeat`, `set_my_heartbeat` | all turns (`crates/server/src/heartbeat_instructions.rs`, `agent_heartbeat.rs`) — channel-scoped instruction overrides only mean something on Discord, but the `*_my_heartbeat` pair (an agent's own enable flag and interval) applies anywhere |
| **Nostr identity** | `nostr_generate_key`, `nostr_list_keys`, `nostr_switch_identity` | all turns (`crates/server/src/system_actions.rs`) — bootstrap tools, exposed **before** any key exists (see [Nostr](#nostr)) |
| **Nostr passthrough** | `nostr_run` | all turns (`crates/server/src/system_actions.rs`) — thin nostaro passthrough (see [Nostr](#nostr)) |
| **Nostr relay target** | `get_my_nostr_relay`, `set_my_nostr_relay` | all turns (`crates/server/src/agent_nostr_relay.rs`) — where the agent's inbound Nostr events get mirrored (a Discord webhook URL); also editable from the dashboard |
| **Nostr messaging** | `nostr_post`, `nostr_reply`, `nostr_dm`, `nostr_zap`, `nostr_upload` | Nostr turns only (`crates/nostr`) — the reply path used while handling an inbound Nostr event; `nostr_dm` and `nostr_zap` carried a trusted-only gate until #306 dropped it (see [Nostr](#nostr)) |
| **Discord** | `discord_list_guilds`, `discord_list_channels`, `discord_channel_config`, `discord_add_reaction`, `discord_send_file`, `discord_create_channel` | Discord turns only (`crates/discord`) |
| **Delivery** | `send_ui`, `request_peer_review` | all turns whose transport can render UI (`send_ui`) or send text (`request_peer_review`) — implementations in `crates/actions/src/a2ui.rs` and `crates/server/src/peer_review.rs`; the transport only supplies the surface (`GatewayActions::a2ui_surface` / `text_delivery`) |
| **Webhook targets** | `list_webhooks`, `get_default_webhook`, `set_default_webhook`, `list_subtask_webhooks`, `get_default_subtask_webhook`, `set_default_subtask_webhook` | all turns (`crates/server/src/webhook_targets.rs`) |
| **Webhook creation** | `discord_create_webhook`, `ensure_webhook`, `ensure_subtask_webhook` | Discord turns only (`crates/discord`) — the `ensure_*` pair creates a webhook through serenity when no default exists |
| **Voice** | `join_voice_channel`, `leave_voice_channel` | Discord turns only (`crates/discord`) |
| **Skills** | `create_skill` | all turns (`crates/server/src/agent_management.rs`) |

The rows above name every action in `SystemGatewayActions::own_definitions()` (the "all turns"
rows) and every action in `DiscordGatewayActions::definitions()`. Each action is defined in
exactly one place: `cancel_subtask`, the two heartbeat-instruction actions, the six
webhook-target actions, `create_skill` and `request_peer_review` used to be defined by Discord
as well, but #157 S2/S3/S5/S6/S7 removed those definitions so the transport-independent
implementation is the only one. The three Nostr bootstrap tools are the reverse case: they are
defined by both `SystemGatewayActions` and `crates/nostr`, and `definitions()` de-duplicates by
name so the transport-independent one wins — which is what makes them usable on turns where no
Nostr gateway is running yet. Drift is caught by tests rather than by
review — `server_gateway_action_table_matches_own_definitions`
(`crates/server/src/system_actions.rs`) parses the "all turns" rows of this table and requires
them to equal `own_definitions()` in both directions,
`server_tools_are_classified_for_dispatch` requires every own definition to be classified and
rejects dead names on the constant side, and `test_definitions_returns_expected_count` pins the
Discord set (including that the actions relocated in #157 S1/S2/S3/S5/S6/S7 are no longer
defined there). Note the gap: only the "all turns" rows are checked against this file. The
`Discord turns only` / `Nostr turns only` rows are **not** — the transport-side tests pin their
definition sets against constants, not against this table, so those rows can still go stale
without a test failing.

Visibility is not uniform. `configure_*`, `manage_allowed_commands`,
`update_heartbeat_instructions` and the core action `update_instructions` are owner-only —
that is `OWNER_ONLY_ACTIONS` in full. `nostr_list_keys`, `nostr_switch_identity`,
the `*_my_nostr_relay` and `*_my_heartbeat` pairs,
`read_heartbeat_instructions`, the voice actions and `create_skill` are trusted-only, meaning
they are neither listed nor executable on a turn driven by an untrusted external user. That gate
is what keeps an inbound Nostr note from talking the agent into swapping its own key:
`nostr_switch_identity` is unreachable from a `caller=Agent` turn, and the passthrough refuses
`init`, so neither adopting nor minting-over a key is possible from inbound. It does **not** bound
what an inbound turn can send or spend, and it is not meant to: `nostr_run` carries no caller gate
(#303) and the passthrough denies only `init`, `watch` and `relay`, so `nostr_run zap` and
`nostr_run dm` have gone through ever since #303 — before that, `nostr_run` itself was
trusted-only, so the whole passthrough was out of reach from a `caller=Agent` turn. Gating the
inner `nostr_zap` / `nostr_dm` only changed which tool names got listed, so #306 dropped that
gate rather than adding a matching one to the passthrough — the consistency is taken in the
direction of fewer constraints, and whether to send a DM or a zap is the agent's own call.
`nostr_generate_key` is deliberately *not* gated: it only mints a key that nobody has
adopted yet, and adopting one is what `nostr_switch_identity` gates. The single table is
`crates/actions/src/bridge.rs` (`OWNER_ONLY_ACTIONS` / `TRUSTED_ONLY_ACTIONS`), consulted by both
tool listing and execution.

`create_skill` (a gateway action, `source_type="acquired"`) and `create_my_skill` (a core
action, `source_type="self_created"`, `situation_pattern` required) are deliberately kept as
two separate tools: #157 only moves generic implementations out of the transport layer, and
dropping a tool name would break calls recorded in past conversation logs.

## Nostr

The Nostr gateway is per-agent, like the Discord one: each agent has its own key, its own relay
set and its own subscription filter, persisted in the DB. Nostr protocol work is not
reimplemented here — `crates/nostr` drives the `nostaro` CLI as a subprocess, with a config path
pinned per agent (`data/agents/{id}/nostr/config.toml`) so one agent can never sign with
another's key.

**An agent can put itself on Nostr.** `nostr_generate_key`, `nostr_list_keys` and
`nostr_switch_identity` are defined by `SystemGatewayActions`, so they exist *before* the agent
has a key and on turns where no Nostr gateway is running (the latter two on trusted turns only).
The agent generates a key (optionally with a vanity npub prefix), looks up the resulting npub,
and adopts it — and adopting is what starts the gateway: if the agent was not connected yet,
`nostr_switch_identity` also writes a bounded filter and brings the connection up. That filter is
`keywords = [npub]` (`crates/nostr/src/manager.rs`), which becomes
`nostaro watch --keyword=<npub>` — a **body keyword match, not a `#p` tag subscription**. See the
coverage note at the end of this section for what that costs.
No dashboard step and no restart in between. The private key is
generated and stored server-side (mode 0600) and is never returned to the model; `nsec` is
masked out of tool results and error strings.

**`nostr_run` is a thin passthrough, not a wrapper.** It forwards a `nostaro` subcommand and its
arguments straight through, so once the agent has adopted a key (no `config.toml` means no
passthrough — the tool errors out instead of spawning nostaro), whatever nostaro can do the agent
can do: arbitrary-kind `event`,
`profile` (kind:0), NIP-28 `channel` creation and posting, `upload`, `react`, `repost`,
`follow`, `get` / `timeline` / `search`, and so on. Only three subcommands are denied —
`init` (key creation/overwrite, which belongs to the tools above), `watch` (unbounded inbound,
which belongs to the gateway) and `relay` (it would edit `config.toml` only, desyncing from the
DB that owns relay settings and silently evaporating on the next gateway start). Everything else
is passed through unexamined.

What OpenCrab guarantees about that passthrough is deliberately just two things:

1. **No key mix-up between agents** — the config is always the calling agent's
   (`--config` cannot be overridden through the arguments), and the command is built as
   structured argv, never as a shell string.
2. **`nsec` stays hidden** — the agent never handles a private key, and both stdout and error
   output are masked.

Judgements about the Nostr protocol itself — which kind, which tags, what a valid event looks
like — are left to nostaro. That is the point: OpenCrab does not reimplement the spec, so a
nostaro capability does not need a matching OpenCrab tool before an agent can use it.
`docs/nostaro-interface.md` records the subcommands and output shapes this crate relies on.

Inbound events matching the agent's filter can also be mirrored into a Discord channel via
webhook. The target is set from the dashboard, or by the agent itself with `get_my_nostr_relay`
/ `set_my_nostr_relay`.

**Inbound coverage is not complete, and the cause is on this side.** The bootstrap filter written
by `nostr_switch_identity` matches the agent's npub as a **keyword in the note body**, so a reply
that only references the agent through `e` / `p` tags — which is what a normal Nostr client
produces — matches nothing and is dropped **systematically**, not occasionally. Fixing it takes
changes in both places: the filter this repo writes (`kojira/opencrab#271`) and the subscription
nostaro builds from it (`kojira/nostaro#6`, unmerged). Until then, do not assume every mention
produces a turn. Outbound — posting, and replying to an event the agent did receive — is the part
that is exercised today.

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

- **Agent settings** — `heartbeat_interval_secs`, `heartbeat_enabled`, `heartbeat_min_interval_secs` (the floor `set_my_heartbeat` enforces, 300; the ceiling is a code constant of 24h), `workspace_path`, `max_workspace_size_mb`. Watch the two layers of default: the shipped `config/default.toml` sets `heartbeat_interval_secs = 1800` and `heartbeat_enabled = true`, while the code fallbacks used when a key is *absent* are 29 and `false` (`crates/server/src/config.rs`). These are the global values; an agent that has opted in via `set_my_heartbeat` fires on its own stored interval instead (the dashboard's heartbeat fields under Agent Channels are the older per-Discord-channel setting, a separate table)
- **LLM providers** — Default provider and default model (`[llm] default_provider` / `default_model`), per-use-case model selection, fallback chains, model aliases, self-selection toggle. Provider settings are also editable from the dashboard and saved to the DB, which then takes precedence over this file
- **Gateway settings** — REST port (8080), per-agent Discord token (DB-persisted), per-agent Nostr key/relays/filter (DB-persisted), CLI toggle
- **Database** — SQLite path
- **Tools** — Shell commands with per-command permission levels (`agent` / `owner`)
- **Background tool execution** — `[subtask] auto_dispatch` (default `true`). Tool calls run in the background so the response loop never blocks; results are re-injected into the conversation when they finish. Set it to `false` (or export `OPENCRAB_SUBTASK_AUTO_DISPATCH=0`, which takes precedence) to fall back to fully synchronous tool execution. See "非ブロックツール実行" in `docs/DESIGN.md` for which tools stay inline and why.

The `[tools]` section supports config-driven tool definitions with permission levels, hot-reloaded without server restart.

## Testing

```bash
# Run all tests. Some crates gate modules behind features (e.g. discord), so
# --all-features is what CI runs and what the counts below refer to.
cargo test --workspace --all-features

# Run tests for a single crate (see [workspace] members in Cargo.toml for the full list)
cargo test -p opencrab-db
cargo test -p opencrab-actions
cargo test -p opencrab-llm
cargo test -p opencrab-core
cargo test -p opencrab-web-gateway

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
| Discord | serenity (per-agent gateway) |
| Nostr | `nostaro` CLI driven as a subprocess (per-agent config/key) |
| Serialization | serde / serde_json |
| Error Handling | anyhow / thiserror |
| Logging | tracing / tracing-subscriber |
| Config Watch | notify_debouncer_mini |

## License

MIT
