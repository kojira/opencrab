# Design: Discord slash-command model switching

Issue: [#1004](https://github.com/kojira/opencrab/issues/1004)

Status: implementation-ready

## 1. Purpose and requirements

Add a global Discord application command that lets an owner inspect and persistently change the bound agent's LLM model without turning the command into an agent prompt.

The implementation must:

- add a generic gateway-to-core `command` request to the extgate wire protocol;
- dispatch commands through an extgate command registry, with built-ins `list_models`, `set_model`, and `reset_model`;
- keep Discord concepts and dependencies out of core, extgate, gate-client, and actions;
- reuse the server's model-choice source and the existing `check_agent_model_change` plus `apply_agent_patch` write path;
- persist an explicit selection in `agents.model`, apply it beginning with the next turn, and clear it on reset so the server default becomes effective;
- authorize only `SaidCaller::Owner`; and
- expose old-core `unknown_message` failures to the Discord user rather than silently falling back to chat.

This is not a session override. The selected model remains effective across reconnects and process restarts until changed or reset.

## 2. Out of scope

- Parsing `/model` or another prefix from ordinary text messages.
- Nostr, web, or CLI adapter implementations.
- Registering model pricing from a gateway.
- Adding Discord types, IDs, libraries, or branches to shared/core crates.
- Fuzzy matching in the core resolver.
- Changing provider configuration, server defaults, runtime thinking settings, or Discord intents.
- Treating CoAgent as an owner for these commands.

## 3. Discord UX

Register one global command on every Serenity 0.12 `ready` event:

```text
/model list
/model set model:<model>
/model reset
```

`model` is a required string option with autocomplete enabled. Registration is an upsert of the same global command definition and is safe on reconnect.

All final responses are ephemeral. `list` is deferred ephemerally before the core request. `set` and `reset` are always deferred ephemerally before the core request. The adapter edits the deferred response with exactly one of these texts:

| Outcome | Exact Discord text |
|---|---|
| list success | `Available models:\n{lines}\n\nCurrent: {current}\nDefault: {default}` |
| list with no models | `No models are currently available.\n\nCurrent: {current}\nDefault: {default}` |
| set success | `Model set to `{model}`. It will be used from the next turn.` |
| reset success | `Model reset to the server default `{default}`. It will be used from the next turn.` |
| unauthorized | `Only an owner can use /model.` |
| exact name not found | `Unknown model `{input}`. Choose a model from autocomplete or use provider:model.` |
| ambiguous bare name | `Model `{input}` exists for multiple providers. Use provider:model.` |
| missing pricing or another model validation failure | `Could not set model: {server_message}` |
| old core | `This server does not support /model yet (unknown_message).` |
| timeout | `The model command timed out. Try again.` |
| all other failures | `The model command failed: {message}` |

In the table, `{lines}` is the lexicographically sorted list of canonical `provider:model` values, one backticked value per line. `{current}` is the effective model and `{default}` is the server default, each backticked. `{server_message}` is the exact message returned by `check_agent_model_change`; in particular, the existing dashboard error for a missing `model_pricing`/`context_window` registration must pass through unchanged. No Discord code may invent pricing data or offer registration.

The backticks shown around placeholders are literal formatting. Dynamic values must be escaped so they cannot create mentions or additional Markdown structure. If Discord's message limit would be exceeded, `list` includes as many complete sorted lines as fit and ends the model section with `… and {count} more.`

### 3.1 Autocomplete

Autocomplete filters only a gateway-local snapshot returned by `list_models`. Matching is case-insensitive substring matching against canonical `provider:model` and the bare model portion; canonical-prefix matches sort first, then bare-prefix matches, then other substring matches, with canonical value as the final lexical tie-breaker. Values sent to Discord are always canonical `provider:model` strings.

Return at most 25 choices, each with the same canonical string as `name` and `value`. Truncate neither value nor model identity; omit a choice that exceeds Discord's 100-character choice limit.

The local cache TTL is exactly 60 seconds per binding. On an expired or absent entry, autocomplete gives the core request 1,500 ms. On timeout or error it returns zero choices; it does not send an interaction error and does not use data older than 60 seconds. A successful `set` or `reset` does not invalidate the model-list cache because those operations do not change availability.

## 4. Invariants and data ownership

- `agents.model` is the only persisted per-agent selection. A set stores the canonical `provider:model`; reset stores SQL `NULL` through `AgentPatch.model = Some(None)`.
- The effective model is the explicit `agents.model` value when present, otherwise the server default.
- A successful mutation affects only turns started after the commit. An already-running turn keeps the model it resolved at turn start.
- The server's existing model-choice logic is the only availability source. Gateways do not query providers or maintain an independent catalog.
- Resolution is exact over that returned list: first match the entire case-sensitive canonical `provider:model`; if the input contains no provider separator, match a case-sensitive bare model ID only when exactly one provider supplies it. Zero matches are `model_not_found`; multiple bare matches are `model_ambiguous`.
- Whitespace around `args.model` is rejected rather than normalized. Fuzzy and substring matching exist only in Discord autocomplete filtering.
- `check_agent_model_change` runs before every effective change. Missing `model_pricing` is a validation failure with the same message shown by the dashboard path.
- `apply_agent_patch` is the only model write. There is no write to provider settings or a runtime/session override.
- The binding determines the target agent. A request cannot supply an agent ID.

## 5. Wire protocol and command contract

The additive inbound frame has this exact shape:

```json
{
  "m": "command",
  "id": "01JCOMMANDREQUESTID",
  "binding_id": "binding-uuid",
  "caller": { "role": "owner" },
  "name": "list_models",
  "args": {}
}
```

`id` is the request/reply correlation ID and must be unique among requests sent on one connection. `binding_id` must identify an acknowledged binding on that connection. `caller` uses the existing `SaidCaller` JSON representation. `name` is a registry key and `args` is always a JSON object.

Replies reuse the existing frames:

```json
{"m":"ok","id":"01JCOMMANDREQUESTID","result":{}}
```

```json
{"m":"err","id":"01JCOMMANDREQUESTID","code":"error_code","message":"human-readable message"}
```

Unknown request fields remain ignored under existing additive-field rules. A malformed frame, a non-object `args`, or missing required field uses the existing protocol malformed-frame behavior. An unknown command name returns `unknown_command` and never enters the ordinary said path.

### 5.1 `list_models`

Request:

```json
{
  "m": "command",
  "id": "cmd-list-1",
  "binding_id": "binding-uuid",
  "caller": { "role": "owner" },
  "name": "list_models",
  "args": {}
}
```

Success:

```json
{
  "m": "ok",
  "id": "cmd-list-1",
  "result": {
    "models": ["anthropic:claude-sonnet-4", "openai:gpt-5"],
    "configured_model": "openai:gpt-5",
    "current_model": "openai:gpt-5",
    "default_model": "anthropic:claude-sonnet-4"
  }
}
```

`configured_model` is nullable when `agents.model` is unset. `current_model` and `default_model` are canonical strings and are required. `models` is sorted and deduplicated by canonical string.

### 5.2 `set_model`

Request:

```json
{
  "m": "command",
  "id": "cmd-set-1",
  "binding_id": "binding-uuid",
  "caller": { "role": "owner" },
  "name": "set_model",
  "args": { "model": "openai:gpt-5" }
}
```

Success:

```json
{
  "m": "ok",
  "id": "cmd-set-1",
  "result": {
    "configured_model": "openai:gpt-5",
    "current_model": "openai:gpt-5",
    "default_model": "anthropic:claude-sonnet-4",
    "applies": "next_turn"
  }
}
```

The result returns the canonical resolved model, including when the request used a unique bare ID.

### 5.3 `reset_model`

Request:

```json
{
  "m": "command",
  "id": "cmd-reset-1",
  "binding_id": "binding-uuid",
  "caller": { "role": "owner" },
  "name": "reset_model",
  "args": {}
}
```

Success:

```json
{
  "m": "ok",
  "id": "cmd-reset-1",
  "result": {
    "configured_model": null,
    "current_model": "anthropic:claude-sonnet-4",
    "default_model": "anthropic:claude-sonnet-4",
    "applies": "next_turn"
  }
}
```

### 5.4 Stable errors

All three built-ins use these stable codes and messages where applicable:

```json
{"m":"err","id":"cmd-1","code":"forbidden","message":"Only an owner may use model commands."}
{"m":"err","id":"cmd-1","code":"binding_not_found","message":"The binding is not available."}
{"m":"err","id":"cmd-1","code":"invalid_args","message":"Invalid command arguments."}
{"m":"err","id":"cmd-1","code":"model_not_found","message":"The requested model is not available."}
{"m":"err","id":"cmd-1","code":"model_ambiguous","message":"The model ID is available from multiple providers."}
{"m":"err","id":"cmd-1","code":"model_validation_failed","message":"{check_agent_model_change message}"}
{"m":"err","id":"cmd-1","code":"internal","message":"The model command could not be completed."}
```

The adapter uses its more specific Discord templates in §3 for known codes. Logs may include internal detail, but `internal` must not expose secrets, SQL, provider credentials, or filesystem paths.

## 6. Strict permissions

Authorization is intentionally stricter than existing owner-equivalent checks:

| `SaidCaller` variant | Result |
|---|---|
| `Owner` | allowed |
| `CoAgent` | `forbidden` |
| `TrustedUser` | `forbidden` |
| `Agent` | `forbidden` |

This check runs in extgate before command lookup, model listing, resolution, validation, or mutation. It must use an exact `SaidCaller::Owner` match and must not call `is_owner_equivalent`. The Discord adapter still uses existing `caller_for(access, user.id)` so core enforcement does not depend on the adapter hiding commands.

## 7. Registry and server boundary

Extgate owns a generic command registry keyed by command name. Registry handlers receive the resolved binding context, asserted caller, and JSON object arguments; they return JSON or a stable command error. Built-ins are registered explicitly during extgate construction. Duplicate names are a startup error, not last-writer-wins behavior.

The actions boundary exposes a generic, Discord-free model administration capability used by the built-ins. The server implementation:

1. resolves the binding's agent;
2. lists models with the same logic as the existing model-choices API;
3. reads the persisted and effective model;
4. resolves `set_model` exactly against that list;
5. invokes `check_agent_model_change`; and
6. commits with `apply_agent_patch`.

No command registry handler may duplicate the server validation or write SQL directly.

## 8. Lifecycle and state machine

1. On Discord `ready`, upsert the global `/model` definition. A registration failure is logged and retried only on the next `ready` event.
2. `interaction_create` receives a command or autocomplete interaction; message handling remains unchanged.
3. The receive layer writes a new interaction event kind through the existing `on_line` JSON boundary. Parsing that event never produces ordinary message text.
4. The run layer resolves the existing binding and calls `caller_for` with the Discord user ID.
5. For autocomplete, use a fresh cache entry or issue `list_models` within 1,500 ms, then filter locally and answer once.
6. For command submission, defer ephemerally, create one command ID, and send one command frame.
7. Extgate accepts `command` only in the running protocol state and only for an acknowledged binding owned by this connection.
8. Extgate performs the exact-owner check, dispatches through the registry, and emits one `ok` or `err` with the same ID.
9. The server reads or mutates the agent. A successful mutation commits before `ok` is emitted.
10. Discord edits the deferred response. A response arriving after the adapter timeout is ignored and cannot cause a second interaction edit.

The submission timeout is exactly 10 seconds from sending the command frame. There is no automatic transport retry for any command. If a reply is lost, Discord shows the timeout text and the user may submit again.

`set_model` and `reset_model` are target-state idempotent: repeating a successful set to the same canonical value or reset when already unset succeeds with the same result and creates no additional semantic state. `list_models` is read-only. Every user retry uses a fresh request ID; the protocol does not replay responses across reconnects.

## 9. Failures, concurrency, and retries

- Unknown or disabled binding: fail with `binding_not_found`; never infer another binding.
- Unauthorized caller: fail with `forbidden` before all model I/O.
- Empty, non-string, padded, missing, unavailable, or ambiguous model: no write.
- Missing model pricing/context-window data: `check_agent_model_change` rejects the set; surface its dashboard message and leave `agents.model` unchanged.
- Provider listing failure, DB error, or patch failure: return a non-secret error and leave persisted state transactional.
- Two concurrent sets serialize through the existing DB write behavior. Each success means its value committed; the last committed write is the persisted value. No compare-and-swap guarantee is added.
- Reset racing with set follows the same last-commit rule.
- A turn that has already selected its model is isolated from a concurrent change. The next turn reads the final committed value.
- Discord reconnect may register the same command again but must not resend an already-submitted mutation.
- Autocomplete timeout is silent and returns zero choices; command timeout is visible and is never reported as success.

## 10. Compatibility, migration, and rollback

This is an additive wire change with no schema migration and no data backfill. Existing nullable `agents.model` rows already represent explicit selection versus server-default fallback.

A new core accepts old gateways unchanged. A new gateway connected to an old core receives the existing `unknown_message` error for `m:"command"`; Discord must display the exact old-core text from §3. It must not retry as `said`, call REST, or mutate local state.

The command frame and built-ins are not gateway operation declarations. Therefore the operation `declaration_digest` algorithm and values are unchanged. The frame also adds no extgate configuration field, so `config_digest` computation and values are unchanged. Slash-command registration state, autocomplete cache contents, command IDs, and registry order must not enter either digest. Existing digest mismatch and reconnect behavior remain unchanged.

Rollback is code-only: deploy the prior core and Discord gateway. Persisted `agents.model` values remain valid and continue to drive normal model selection. During a mixed-version rollback, new gateways show the old-core error; old gateways ignore the new capability. Operators may reset a persisted model through the existing dashboard before or after rollback. No wire downgrade, data rewrite, or Discord intent change is required.

## 11. Per-crate change list

- `crates/gate-client` — add the `command` request frame, typed `ok`/`err` completion, one-shot request correlation, and the 10-second caller-supplied timeout path; no model or Discord policy.
- `crates/extgate` — parse and dispatch `command` in running state, validate acknowledged binding, enforce exact `SaidCaller::Owner`, host the generic registry, register three built-ins, and preserve existing unknown-message behavior.
- `crates/actions` — add the small Discord-free model administration interface/result types consumed by extgate.
- `crates/server` — implement that interface by reusing model-choice listing, persisted/effective agent reads, `check_agent_model_change`, and `apply_agent_patch`.
- `crates/discord-gateway` — Serenity 0.12 global registration, `ready`, `interaction_create`, new `on_line` interaction event, `caller_for` reuse, cache/filter/autocomplete, deferred ephemeral replies, and error mapping.
- `crates/core` — no Discord command or type; only shared APIs strictly required by the existing validation call may be reused, not forked.
- `crates/gateway` — unchanged unless a neutral shared error type is already the established dependency direction; no Discord additions.
- `crates/db` — no schema change; use the existing agent query/patch APIs.
- `crates/nostr-gateway`, `crates/web-gateway`, `crates/cli-gateway` — no adapter implementation.

## 12. TDD: RED assertions first

### Gate client and extgate

1. A `command` frame round-trips `id`, `binding_id`, `caller`, `name`, and object `args` through serialization.
2. Running state dispatches each registered built-in and correlates exactly one `ok`/`err` by ID.
3. Pre-running state, unknown binding, unacknowledged binding, malformed args, and unknown command fail without invoking a handler.
4. Owner reaches the handler; CoAgent, TrustedUser, and Agent each receive stable `forbidden` and cause zero model-list/read/write calls.
5. Duplicate command registration fails deterministically.
6. An old-core `unknown_message` is preserved as a typed gate-client error.
7. Timeout removes pending correlation; a late reply is ignored and no retry is sent.

### Resolver and server

1. `list_models` returns sorted, deduplicated canonical models plus configured, current, and default values.
2. Full `provider:model` exact match wins before bare-ID handling.
3. A unique bare ID resolves to its canonical model; duplicate bare IDs return `model_ambiguous`; case differences, whitespace, and fuzzy substrings return `model_not_found`/`invalid_args` without writes.
4. Successful set calls `check_agent_model_change` and persists exactly one canonical `agents.model` value through `apply_agent_patch`.
5. Missing pricing returns `model_validation_failed` with the exact dashboard validator message and leaves the row unchanged.
6. Reset persists `NULL`, reports the server default as current, and does not change that default.
7. Repeated same-value set and repeated reset are successful target-state no-ops.
8. A running turn retains its selected model while the next turn observes the committed change.
9. Concurrent set/reset tests prove transactional writes and documented last-commit behavior.

### Discord adapter

1. `ready` constructs exactly the global command and three subcommands specified in §3; reconnect upserts it again.
2. `interaction_create` routes commands and autocomplete through the interaction event kind, never through `said`.
3. All command responses are ephemeral; set/reset are deferred before command I/O.
4. `caller_for` classifications are forwarded unchanged.
5. Autocomplete ordering, case-insensitive filtering, 25-choice cap, 100-character omission, 60-second TTL, and 1,500 ms miss timeout are deterministic under a fake clock.
6. Submission uses one request, a fresh ID, no automatic retry, and a 10-second timeout.
7. Every stable core error maps to the exact Discord text in §3, including `unknown_message` and the unchanged validation message.
8. Message-limit truncation emits only complete model lines and the exact remaining-count suffix.
9. Existing message receive behavior and configured intents are byte-for-byte/bit-for-bit unchanged.

## 13. Acceptance and Discord QC

Automated acceptance:

- focused wire, extgate registry, server model, and Discord interaction tests pass;
- formatting, lint, and workspace tests pass;
- protocol fixtures prove old frames still decode and old-core `unknown_message` remains visible;
- no Discord dependency or symbol appears in core, extgate, gate-client, actions, gateway, or DB;
- no migration or schema diff is present.

Manual Discord QC in a test application:

1. Confirm `/model` appears globally with `list`, `set`, and `reset`, and no extra command or option.
2. Confirm autocomplete responds within Discord's interaction deadline, returns at most 25 canonical choices, filters as specified, and safely returns no choices when core is unavailable.
3. As an Owner, list models, set by autocomplete, set by a unique bare ID sent through a test interaction, and reset; verify ephemeral exact text and dashboard-visible `agents.model` state.
4. Verify a new turn uses the changed model, an in-flight turn does not change, and reset uses the server default on the next turn.
5. As CoAgent, TrustedUser, Agent, and an unclassified user, verify the exact owner-only text and zero DB changes.
6. Attempt an ambiguous bare ID, unknown model, padded model, and model lacking pricing; verify exact errors and unchanged persistence.
7. Connect the new Discord gateway to an old core and verify the explicit `unknown_message` text with no chat prompt produced.
8. Reconnect Discord, invoke repeated set/reset, and verify one response per interaction, no duplicate mutation, unchanged intents, and no public response.

未決事項: なし
