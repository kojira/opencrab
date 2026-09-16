# CLI gateway design

Tracking issue: [#994](https://github.com/kojira/opencrab/issues/994)

## 1. Decision

Add a standalone `opencrab-cli-gateway` process with two frontends over the existing external-gateway protocol:

- **REPL mode** for an interactive terminal.
- **JSONL mode** for scripts and long-running local integrations.

The gateway is a concrete outer process. It converts terminal input to generic V3 `said` frames and generic core output to terminal events. It does not add a CLI branch to core, routing, conversation assembly, persistence, or output filtering.

This is separate from the existing `opencrab-cli` database-management REPL. That binary and its behavior remain unchanged.

## 2. Scope and invariants

### In scope

- Text conversations with one selected agent and one selected session per process.
- Creating a new core-owned session binding or attaching to an existing binding address.
- Normal replies, `NO_REPLY`, activity, turn failure, and later background-completion replies.
- Existing UDS reconnect and binding replay behavior.
- Human-readable REPL output and machine-readable JSONL output.

### Invariants

- `core`, `actions`, `db`, `extgate`, `gate-client`, and `server` do not interpret `cli` as a platform value.
- Conversation history, admission, session serialization, background completion, delivery, and DB state remain core-owned.
- The CLI gateway never reads or writes the core DB directly.
- The server does not spawn, restart, or supervise the CLI gateway.
- The gateway has no gateway-specific database and no production fallback.
- Input is never automatically replayed after an uncertain disconnect.
- The existing `NO_REPLY` and `CompletedNoReply` contracts are preserved; no marker, heuristic, or state flag is added.

## 3. Process and configuration

### 3.1 Command

```text
opencrab-cli-gateway \
  --placement /path/to/cli-placement.json \
  --agent <exact-agent-id> \
  (--new <session-name> | --session <session-address>) \
  [--mode auto|repl|jsonl] \
  [--connect-timeout-secs 10]
```

`--agent` is an exact ID, not a fuzzy name lookup. Exactly one of `--new` and `--session` is required.

Mode selection:

- `auto`: REPL only when both stdin and stdout are terminals; otherwise JSONL.
- `repl`: requires terminal stdin and stdout, otherwise startup fails.
- `jsonl`: never prints prompts or non-JSON data to stdout.

### 3.2 Placement

The operator supplies a non-secret placement file:

```json
{
  "core_socket": "/absolute/path/to/gate.sock",
  "instances": [
    {
      "instance_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
      "revision": 1,
      "agent_id": "agent-a",
      "author_id": "local-operator"
    }
  ]
}
```

Validation is fail-loud:

- `core_socket` is absolute.
- `instances` is nonempty.
- instance IDs are canonical lowercase UUIDs.
- revisions are positive.
- `agent_id` and `author_id` are nonempty.
- instance and agent IDs are unique within the file.
- the selected agent occurs exactly once.

The hello digest is computed from byte-exact canonical config `{"author_id":<JSON string>}`, matching the generic web-gateway pattern. The gate instance and subject mapping are provisioned by the operator before launch; this command does not hold an operator Bearer token and does not provision an instance.

One core instance permits one live gateway connection. A second process using the same instance is rejected by the existing `instance_active` rule. Multiple simultaneous terminal processes therefore require separately provisioned instance IDs; automatic instance creation is out of scope.

### 3.3 Session selection

`--new NAME`:

1. generate a binding UUID;
2. use `extgate-<binding-uuid>` as the address/session ID;
3. call the existing `InstanceClient::create_binding` with `NAME` as the session theme;
4. wait until the binding is acknowledged before accepting input;
5. print/emit the reusable session address.

`--session ADDRESS`:

- does not create or mutate a binding;
- succeeds only when the selected instance remembers that exact address and the binding is acknowledged;
- fails with `session_unavailable` after the connection timeout if the address is absent or belongs elsewhere.

This preserves core binding ownership and avoids a separate CLI session registry.

## 4. Identity and authorization

The placement is installed and launched by the local operator. Accepted terminal input is mapped to:

```text
caller = owner
author_id = placement.instances[].author_id
author_label = null
start_turn = true
system_context = null
reply_target = null
live_inbound_scope = all
```

No token, API key, or credential is accepted in argv, stdin, stdout, logs, or the placement file. UDS filesystem ownership/mode and pre-provisioned instance/binding authorization remain the trust boundary. Remote shell or multi-user terminal authentication is not added.

## 5. REPL experience

After connection and binding acknowledgement:

```text
Connected: agent-a / extgate-…
you> hello
accepted #1
agent-a> Hello.
you>
```

One nonempty line is one message. Empty lines are ignored.

Reserved commands:

| command | behavior |
|---|---|
| `:help` | show commands and line rules |
| `:status` | show selected agent/session and connected/disconnected state |
| `:quit` | begin graceful shutdown |
| `::text` | send a message whose text begins with `:`, as `:text` |

The gateway generates a UUID for every REPL message and uses origin `cli:<uuid>`.

A single renderer owns stdout. When an event arrives while `you> ` is visible, it clears the prompt line, prints the event, and redraws the prompt. No two tasks write stdout directly.

User-visible events:

- `read`: `read`
- `started`: `thinking…`
- normal `Message`: `<agent-id>> <text>`
- `ended`: clear the activity display; do not invent a reply
- `CompletedNoReply`: `(no reply)`
- `TurnFailed`: `(turn failed)`
- disconnect/reconnect: one state line per transition
- later completion `Message`: print immediately and redraw the prompt

`Completed { target }` is not useful to a terminal user and is not printed in REPL mode. It remains available in JSONL mode.

The REPL does not show internal tool JSON, prompts, tokens, secrets, raw wire frames, or stack traces.

## 6. JSON Lines contract

### 6.1 Framing

- stdin and stdout are UTF-8 JSON object plus LF.
- Maximum input line is 1,048,576 bytes including LF.
- Duplicate object members, non-object JSON, invalid UTF-8, missing LF beyond the limit, and unknown `type` are rejected.
- Unknown members are rejected in this user-facing contract so misspelled automation fields cannot silently succeed.
- stdout contains only protocol objects. Diagnostics and tracing go to stderr.
- Output object member order is not significant.

### 6.2 Input

Send a message:

```json
{"type":"message","id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","text":"hello"}
```

Rules:

- `id` is a canonical lowercase UUID supplied by the caller.
- `text` is a nonempty UTF-8 string.
- origin is deterministically `cli:<id>`.
- attachments are not accepted in the first release.

Graceful shutdown:

```json
{"type":"shutdown"}
```

No further input is accepted after shutdown.

### 6.3 Output

Ready:

```json
{"type":"ready","agent_id":"agent-a","session_id":"extgate-…","state":"connected"}
```

Accepted input:

```json
{"type":"accepted","id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","origin":"cli:bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","seq":1}
```

Not admitted:

```json
{"type":"not_admitted","id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"}
```

Reply:

```json
{"type":"message","delivery_id":"…","text":"Hello.","reply_origin":"cli:bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"}
```

`reply_origin` is a string or `null`; it is not guessed when core reports an ambiguous/background turn.

Activity:

```json
{"type":"activity","activity_id":"…","state":"started","origin":null}
```

The state is the generic gate-client value (`read`, `started`, or `ended`).

Other live events:

```json
{"type":"completed","target":"…"}
{"type":"completed_no_reply","reply_origin":"cli:…"}
{"type":"turn_failed","reply_origin":"cli:…"}
{"type":"connection","state":"disconnected"}
{"type":"connection","state":"connected"}
```

Per-record or runtime error:

```json
{"type":"error","request_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","code":"instance_not_ready","detail":null}
```

`request_id` is `null` if no trustworthy input ID was parsed. Internal paths, SQL, credentials, prompts, and raw provider errors never appear in `detail`.

Final normal closure:

```json
{"type":"closed","reason":"requested"}
```

## 7. Event ordering, concurrency, and backpressure

- One process owns one selected session.
- A bounded ingress queue of 32 messages accepts terminal/JSONL input.
- One sender consumes ingress in input order and calls `post_said_with_self_context`.
- The core remains authoritative for session serialization and its own queue limit.
- Multiple accepted inputs may have turns queued while earlier turns run; the gateway does not create another lock or reorder them.
- Acceptance results are emitted in input order. Live activity and reply events retain core arrival order and may interleave with acceptance events.
- A single bounded output queue of 32 objects serializes all stdout writes.
- When ingress is full, reading pauses; messages are not dropped.
- When stdout is blocked, live-event consumption pauses. Existing gate-client capacity and disconnect behavior remain fail-loud; events are never silently discarded.
- Broken stdout is fatal and terminates the process rather than acknowledging unseen output.

## 8. Background completion

The live-event consumer remains active independently of the input prompt. A completion that starts a later core turn therefore produces new activity and message/no-reply events on the same terminal or JSONL stream.

The V3 protocol intentionally has no catch-up or persistent gateway cursor. After the CLI process exits, later output is not replayed to a new CLI process. Users expecting a background completion must keep the process connected until the desired terminal event arrives.

The gateway cannot know that no future background completion exists after an `ended` event. It therefore makes no “all background work finished” claim.

## 9. Disconnect, reconnect, and idempotency

The existing `InstanceClient` reconnect loop and hello/bind replay are reused.

On disconnect:

1. emit one disconnected event;
2. stop accepting sends to core and return `disconnect` for any input already being processed;
3. do not buffer, replay, or automatically retry input;
4. wait using the existing reconnect backoff;
5. after hello and the selected binding are acknowledged, emit connected/ready and resume input.

A caller that did not observe an acceptance result may resend the same message UUID. The origin remains `cli:<uuid>`, so existing `(binding_id, origin)` dedup returns the original sequence and does not start a duplicate turn. A different UUID is a new message. The CLI never changes an ID during retry.

No reply, say, activity, or external effect is synthesized from a disconnect.

## 10. Shutdown and EOF

- REPL `:quit` and Ctrl-D begin graceful shutdown.
- JSONL `shutdown` and stdin EOF begin graceful shutdown.
- Graceful shutdown stops ingress, drains messages already in the local ingress queue through their said acknowledgements, waits for any **currently started** activity to end or fail, flushes stdout, then exits.
- It does not wait for an unknown future background completion after the current activity has ended. Keeping stdin open is the programmatic way to continue following later completions.
- SIGTERM follows the same graceful path with a bounded 10-second drain, then exits nonzero if the drain did not complete.
- SIGINT exits with status 130 after flushing already serialized output; a second termination signal exits immediately.
- Core turns are not cancelled by CLI shutdown.

## 11. Errors and exit status

Process exit codes:

| code | meaning |
|---:|---|
| 0 | requested shutdown or EOF completed normally |
| 1 | runtime failure: UDS/binding timeout, broken stdout, protocol failure, or failed drain |
| 2 | invalid argv or placement |
| 130 | SIGINT |

JSONL record errors are reported as output events and do not terminate the stream unless framing cannot be recovered safely. Stable gateway-facing codes include `bad_request`, `too_large`, `instance_unavailable`, `session_unavailable`, `instance_not_ready`, `not_admitted`, `conversation_busy`, `disconnect`, `gate_error`, and `output_closed`. Existing wire codes are forwarded only after redacting detail.

TTY errors are short human messages on stderr; JSONL errors are stdout events, with optional diagnostics on stderr. Neither mode exposes secrets or internal identifiers beyond the selected public agent/session and protocol correlation IDs.

## 12. Attachments

The first release is text-only.

- REPL has no attachment command.
- JSONL rejects an `attachments` member.
- File paths, URLs, bytes, MIME inference, and attachment spooling are not implemented.
- Empty text is rejected.

A future attachment design must use the provider-neutral local-file spool contract and define path ownership, byte limits, content type, cleanup, and VLM acceptance before implementation. It must not pass arbitrary local paths to core.

## 13. Security and logging

- Placement/config contain no secret.
- UDS permissions and operator-provisioned instance identity are the authorization boundary.
- No network listener is opened.
- stdout is user/protocol output; tracing always uses stderr.
- Logs may include event type and redacted instance/session labels, but not message text, prompt content, tool payloads, tokens, raw config, environment variables, or full private paths.
- Input text is not echoed by tracing.
- JSONL control characters are escaped by `serde_json`; TTY output does not interpret reply text as ANSI control sequences. Control characters other than newline/tab are rendered safely.
- The public Issue, tests, fixtures, and docs use synthetic IDs only.

## 14. Compatibility and non-goals

Compatibility:

- Protocol remains V3/protocol 2.
- No DB migration.
- No routing, history, prompt, output-filter, completion, `NO_REPLY`, or delivery semantic change.
- Existing Discord, Nostr, web gateway, and `opencrab-cli` behavior is unchanged.
- Core/shared crates do not depend on the new concrete crate.

Non-goals:

- remote TCP/HTTP access;
- terminal authentication or multi-user authorization;
- full-screen TUI, readline history, completion, color themes, Markdown rendering;
- switching agent/session inside one process;
- multiple concurrent stdin clients;
- gateway-owned DB or transcript replay;
- catch-up after disconnect or process restart;
- tool-call JSON display or approval UI;
- attachments, audio, images, or binary stdout;
- provisioning gate instances or storing operator Bearer tokens;
- replacing the existing administration CLI.

## 15. Planned code changes

Implementation is limited to:

- root `Cargo.toml`: add workspace member/dependency for `opencrab-cli-gateway`;
- new `crates/cli-gateway/`:
  - `Cargo.toml`;
  - `src/main.rs`: runtime, tracing-to-stderr, signal ownership;
  - `src/args.rs`: explicit argument/mode parsing;
  - `src/config.rs`: placement validation and digest;
  - `src/runtime.rs`: instance/session lifecycle, bounded ingress/output, reconnect;
  - `src/repl.rs`: prompt renderer and commands;
  - `src/jsonl.rs`: duplicate-safe parsing and output DTOs;
  - focused unit/process tests kept below the unconditional 800-line limit;
- `scripts/check-deps.sh`: include the new crate in concrete-gateway reverse-dependency checks;
- `README.md` and `crates/cli-gateway/README.md`: public usage and mode examples.

`extgate`, core DB/schema, routing, history assembly, output filtering, and platform gateways require no behavior change. `gate-client` changes are not planned; if implementation proves a new generic connection-state API is necessary, work stops and this design is amended/reviewed before adding it.

## 16. Verification

Focused automated checks:

1. argv and placement validation, exact-agent selection, auto mode detection;
2. duplicate-safe JSONL parser, size boundary, UUID/text/unknown-field rejection;
3. stdout contains JSON only in JSONL mode and tracing uses stderr;
4. REPL command escaping, prompt redraw, control-character safety;
5. new/attach binding behavior against a mock core;
6. owner `SaidContext`, deterministic origin, acceptance/not-admitted/wire errors;
7. ordered mapping of Message, Activity, Completed, CompletedNoReply, TurnFailed, and Error;
8. bounded ingress/output backpressure and broken-pipe failure;
9. disconnect with no automatic retry, reconnect/rebind, same-ID dedup;
10. EOF, shutdown, SIGTERM drain, and exit codes;
11. existing `opencrab-gate-client` tests and new crate tests;
12. formatting, dependency-boundary, no-private-identifier, and unconditional 800-line gates.

No full CI is run until implementation and review are complete.

Isolated live acceptance uses a non-production core/DB/UDS and configured real LLM:

- create a new CLI session and obtain one natural REPL reply;
- repeat through JSONL and validate exact stdout objects and empty non-JSON stdout;
- inspect the real `ChatRequest.messages`, persisted session history, said origin, and delivery rather than inferring from UI;
- run one safe background tool completion and confirm initial activity/no-reply, later resumed activity, and final visible result on the still-open CLI;
- retry one uncertain message with the same UUID and prove one persisted inbound/turn;
- disconnect/reconnect and prove no automatic input replay;
- manually verify prompt redraw and graceful terminal exit.

Production is not a fallback. Deployment or production acceptance requires separate explicit authorization.

## 17. Owner decisions

No owner decision remains for implementation of the approved two-mode, text-only first release. Expanding to attachments, multi-session switching, automatic instance provisioning, or a daemon serving multiple terminal clients requires a new design decision and is not implied by this approval.
