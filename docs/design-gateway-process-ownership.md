# Gateway process and storage ownership

Status: **draft design for Issue #1006; not authoritative until architecture review approves it**. If approved, this decision rejects the prior bidirectional ownership and runtime-import design. There is no compatibility fallback.

This is a design-stage correction, not a rejection of every existing component. The generic runtime UDS framing, opaque `kind_id`/config/address storage, generic caller roles, generic exactly-once processing, and dynamically declared operation capabilities are retained because they satisfy the invariants below. Direct core-SQLite access, runtime legacy import, core-owned concrete settings/identities, server concrete administration, and placement-only Discord ownership are rejected and redesigned.

### Relationship to the other architecture documents

This document refines [design-plugin-architecture.md](design-plugin-architecture.md) and [DESIGN.md](DESIGN.md), which use the same terms and target diagram below. “Core owns plugin state/configuration” means only durable **generic conversation/execution state** needed across interchangeable gateways: agents, subjects, sessions, history, session locks, generic schedules and heartbeat state where explicitly decided, subtasks, generic gate rows, and exactly-once ledgers. It does not include a concrete platform's endpoint/account identifiers, credentials, admission or delivery policy, external-identity projection, subscriptions, display behavior, schema, administration, or child lifecycle; the concrete gateway daemon owns those.

Likewise, “transport is send/receive-only” excludes generic conversation execution but does not make a gateway stateless. A gateway owns platform storage, authentication, authorization projection, policy, display behavior, external I/O, and its instance children. Generic external-service supervision utilities may be shared, but `server` neither owns nor supervises a concrete gateway daemon. These ownership decisions are closed here and are not implementation choices.

## 1. Requirements and primary invariant

**New-gateway litmus test:** adding an entirely new concrete gateway requires all of the following:

- zero changes to core, shared, or server source;
- zero core database schema or data migration changes;
- zero core redeployment;
- only a new independently deployed gateway implementation, its gateway-owned schema/admin interface, and generic protocol configuration.

Core sees only opaque `kind_id`, opaque non-secret config bytes, opaque addresses, generic subject/instance/binding/revision identifiers, generic caller roles/events, and dynamically declared operation capabilities. If a future gateway cannot be implemented with the protocol, the remedy is a platform-neutral protocol primitive usable without naming or recognizing that gateway. A concrete core field, table, API, enum variant, prefix branch, or dependency is always rejected.

A concrete gateway is the only owner of its platform configuration, external identities, credentials, authorization projection, policy, display semantics, schema, local administration, and lifecycle. Core treats gateway identifiers and configuration as opaque bytes and never branches on a gateway kind.

```text
operator -- concrete local admin --> gateway daemon --> gateway-owned store/secrets
                                      |       |
                                      |       +-- supervises --> gateway instance child
                                      |                            |
                                      +-- generic gate-admin UDS   +-- generic runtime UDS
                                                   |                            |
                                                   v                            v
                                              core gate admin <---------- core runtime
                                                   |                            |
                                                   +------ core conversation store ---+

server ---------------- generic core APIs only --------------------------> core
       (never configures, spawns, or supervises a concrete gateway daemon)
```

The control-plane UDS, runtime UDS, and each gateway's local admin UDS are distinct paths with mode `0600`. Core's public HTTP server does not expose gate-admin routes.

The dependency direction is one-way:

- `core`, `db`, `gateway`, `actions`, `extgate`, `gate-client`, and `server` have no production dependency on a concrete gateway crate;
- `server` does not spawn, supervise, proxy, configure, or identify a concrete gateway;
- a concrete daemon may depend on generic `gate-client` and shared process utilities, but not on `db`, `server`, or core SQLite types;
- a platform implementation may not depend back on its daemon crate;
- offline migration tooling is the sole exception allowed to depend on `db` and concrete gateway store libraries. It is a separate executable and is never linked into a daemon binary.

## 2. Single owner by data class

| Data class | Canonical owner | Notes |
|---|---|---|
| agents, subjects, sessions, session membership | core | Gateways hold only copied immutable references needed to provision. |
| conversation history, model/tool state, attachments inbox | core | Historical transport-looking text is conversation data, not configuration. |
| session locks, subtasks, generic schedules and heartbeat state | core | Generic execution/conversation coordination remains single-copy in core; platform subscriptions, destinations, and display policy do not. |
| internal agent-to-agent trust relationship | core | No external account identifier is stored with it. |
| generic gate instance ID, opaque kind ID, subject ID, revision, enabled flag, opaque non-secret config bytes/digest | core | Core compares/stores values but does not enumerate or decode kind/config. |
| generic binding ID, instance ID, opaque address, session ID, liveness timestamps | core | Core enforces subject/session membership and address uniqueness only. |
| inbound/outbound exactly-once ledgers and delivery state | core | Existing ledgers and keys are preserved through migration. |
| platform endpoint IDs, account/application/bot IDs, names, filters, relays, channel policy, reactions, delivery settings | the corresponding gateway store | Never named columns or interpreted values in core. |
| external owner/co-agent/trusted identities and their generic role projection | the corresponding gateway store | Gateway authenticates the external ID. For `co_agent`, core also validates the supplied internal agent reference/relation revision against the current core relationship on every execution; revocation is immediate. |
| watches, polling intervals, platform subscription filters | the corresponding gateway store | A gateway may trigger a generic core session; core does not understand the filter. |
| platform credentials and tokens | the corresponding gateway daemon/store | Encrypted at rest; never in opaque core config. |
| gateway child state, backoff, desired/applied generation, reconciliation errors | the corresponding gateway daemon/store | The daemon is the sole lifecycle authority. |
| non-gateway API principals | core `api_principals` | This model has no platform discriminator and cannot contain gateway account IDs. |

### Discord

`discord-gatewayd` owns a Discord SQLite database and a local admin UDS. It owns channel/guild/application/bot-user IDs, channel admission/read/write policy, platform delivery behavior, reactions, owner/co-agent/trusted mappings, bot credentials, and all Discord schema. Generic per-session heartbeat configuration and instructions remain in core; Discord owns only platform watches/subscriptions and display behavior. It supervises one `discord-gateway-instance` child per enabled instance. The current placement-only executable becomes that child; placement files are generated only by `discord-gatewayd` and are not canonical storage.

### Nostr

`nostr-gatewayd` owns a Nostr SQLite database and a local admin UDS. It owns relay/filter settings, watches, public-key mappings, owner/co-agent/trusted mappings, encrypted secret keys, and all Nostr schema. It supervises one `nostr-gateway-instance` child per enabled instance.

### Web and CLI

The Web and CLI gateways remain independently launched generic-runtime clients. Web owns its HTTP bind/authentication, browser identity projection, admission/delivery policy, and display/SSE behavior; CLI owns terminal selection, local display, and input policy. Neither needs a durable platform store today. If either gains durable concrete configuration, its independently deployed daemon/store owns it under the same invariant; it is not added to core or `server`. REST core administration is not a concrete gateway and remains a core API.

No concrete gateway daemon accepts a core database path or a legacy database path, and none opens core SQLite. Placement-only Discord, Web, and CLI executables are instance children or operator-launched instances, not canonical configuration owners; where durable concrete state exists, a gateway daemon owns it.

## 3. Evidence-backed AS-IS violation inventory

This inventory was verified against Issue #1006 and the production source at this design revision. “Production violation” means a normal/build dependency or code compiled into a production target, even if the current launch path does not exercise it. Tests, comments, historical migrations, and persisted conversation text are classified separately so cleanup does not erase evidence or history mechanically.

| ID | Coupling category and concrete path | AS-IS evidence | Classification | Valid component to preserve |
|---|---|---|---|---|
| V01 | Dependency direction — Nostr daemon | `crates/nostr-gateway/Cargo.toml` has normal dependencies on `opencrab-core`, `opencrab-db`, `opencrab-gateway`, and `opencrab-nostr`; `daemon.rs` imports core secret-box, DB, Nostr provisioning, and shared supervisor types. | Production runtime violation. | `opencrab-gate-client`, runtime UDS framing, Nostr parsing/post/watch logic. |
| V02 | Storage/protocol — Nostr daemon | `DaemonConfig` accepts `core_database_path`; `Daemon::new` opens it with `opencrab_db::Db`; reconciliation calls `opencrab_nostr::gate_provision::*` against that connection. | Production runtime violation; bypasses generic gate-admin. | Generic gate-admin operations already implemented in `crates/extgate/src/admin.rs`. |
| V03 | Migration/runtime fallback — Nostr daemon | `DaemonConfig` accepts `legacy_database_path`; startup calls `GatewayStore::import_legacy_once`; `store.rs` reads core-shaped `agent_nostr_config`, `session_watches`, and `trusted_users`. | Production runtime violation. | Gateway-owned store and encrypted-secret conversion, moved to the offline migrator. |
| V04 | Lifecycle — Discord path | `crates/discord-gateway` is a placement-driven one-instance executable and has no owning daemon/store/admin plane. | Production architecture gap. | Discord runtime instance, mapping, operations, token env injection, and runtime UDS client. |
| V05 | Lifecycle utility leakage — shared path | `crates/gateway/src/process_supervisor.rs` is generic behavior but names `discord-gateway`; Nostr daemon consumes it. | Production shared-code vocabulary violation, not a reason to discard the utility. | Restart/backoff/reap implementation after platform-neutral naming and tests. |
| V06 | Core schema/query coupling — Discord/Nostr | Core schema/query production modules contain `channel_config`, mixed `trusted_users.platform`, and `session_watches`; baseline/migration code also contains historical forms. | Current schema/query modules are production violations. Historical migration fixtures are retained evidence until the guarded cleanup; they are not blindly scrubbed. | Generic sessions, membership, heartbeat/schedules, gate rows, and ledgers. |
| V07 | Server API/control plane — Discord/external identities | `server/src/api/channel_configs.rs`, `server/src/api/trusted_users.rs`, and routes in `server/src/lib.rs` administer concrete channel policy and external identity rows through the core DB/public HTTP plane. | Production runtime violation. | Generic agent/session APIs and non-gateway `api_principals`. |
| V08 | Server/shared lifecycle model | `actions/src/agent_gateway.rs` defines DB-restored concrete transport lifecycle/key/identity capabilities; `AppState.gateways` carries that registry. No concrete crate is a normal server dependency now, but this still directs future work toward server-owned lifecycle. | Compiled production architectural coupling; currently no concrete normal dependency from server. | Generic conversation runtime and generic runtime-gate registry in `extgate`. |
| V09 | Timed fire/routing | `actions/src/timed_fire.rs` carries platform-shaped `channel_id`/`guild_id`, static descriptors, and in-process sinks; scheduler comments and state retain old Discord/Nostr distinctions. The production server now registers only generic `ExtgateFire`, which resolves persisted bindings. | Mixed: platform-shaped shared API is a production violation; generic extgate binding resolution is valid and retained. Platform names in `#[cfg(test)]` examples/comments are evidence, not runtime branches. | Core-owned generic schedules/heartbeat, session locks, and `ExtgateTimedFireSink` using binding IDs. |
| V10 | Authorization/dispatch by tool name | `extgate/src/operations.rs` declares sharing/sub-engine and optional `utterance`; `ops_projection.rs` falls back to `is_known_utterance_op(name)`. Existing bridge policy also has name sets for built-ins. | Production extensibility violation for gateway operations: a new operation can need a shared allowlist/classification change. Built-in core tool policy remains core-owned. | Hello-time dynamic declarations, schema validation, live snapshots, generic invoke/callback delivery. |
| V11 | Platform knowledge in shared/server source | Production modules still expose `channel_config`, external `platform`, old gateway lifecycle, and platform-shaped fire fields. Some additional matches are comments describing history or `#[cfg(test)]` fixtures. | Production symbols/branches are violations; explanatory comments/tests are not mechanically deleted unless they compile into behavior or assert the obsolete contract. | Existing no-platform AST audit, expanded to all shared/server production targets. |
| V12 | Web concrete path | `crates/web-gateway` is already an independently launched HTTP/SSE-to-runtime-UDS process using `opencrab-gate-client`, without core DB dependencies. Its placement still carries concrete HTTP/author information and has no daemon-owned admin/store. | Valid for current stateless/operator-placement scope; becomes a violation only if durable concrete policy is put in core/server. | Entire generic runtime client and independent process boundary. |
| V13 | CLI concrete path | `crates/cli-gateway` is independently launched, selects an opaque placement, and talks through the runtime protocol; it has no core DB dependency. | Valid current path. | Entire CLI runtime/client, signal ownership, and terminal UI. |
| V14 | Dev-only QC dependencies | `crates/server/Cargo.toml` normal dependencies contain no concrete gateway crate; dev-dependencies include Nostr/Discord gateway crates and gate-client solely for offline QC harnesses. | Valid test-only dependency. It must stay dev-only and be excluded from production dependency audits, not removed. | Isolated end-to-end QC coverage. |
| V15 | Dead/legacy code and persisted history | `crates/discord`, much of `crates/nostr`, stale design descriptions, old migrations, tests, comments, and stored session/history metadata contain platform vocabulary. Some Nostr library code is still linked by the daemon, so it is not all dead. | Classify by reachability before deletion: linked production coupling must move/remove; dead code may be deleted deliberately; migrations/tests/comments may remain as labeled history; persisted conversation history must remain byte-preserved. | Historical evidence and all generic conversation records. |
| V16 | New-gateway change points | Current lifecycle registry, static timed-fire descriptors, platform-shaped APIs/schema, operation-name fallback, and server routes each invite edits in core/shared/server for a new kind. | Production extensibility violation. | Opaque kind/config/address rows, dynamic hello capabilities, generic gate-admin/runtime protocols. |

The inventory is complete only if the implementation audit enumerates every production Cargo edge and every production AST occurrence in `core`, `db`, `gateway`, `actions`, `extgate`, `gate-client`, and `server`, then classifies each occurrence as V01–V16, valid generic behavior, dev-only QC, dead/legacy, test, comment, historical migration fixture, or persisted history. An unclassified occurrence blocks cleanup and release.

## 4. One-to-one TO-BE transition and completion map

| IDs | TO-BE countermeasure | Transition step | Objective completion evidence |
|---|---|---|---|
| V01 | Nostr daemon depends only on gateway-owned libraries plus generic client/process utilities. | Move secret-box/store helpers behind a Nostr-owned store crate and remove normal core/db/server dependencies. | `cargo tree --edges no-dev` and AST audit show no core/db/server dependency in the daemon. |
| V02 | Nostr provisioning uses gate-admin UDS only. | Replace every direct core SQL provision/read with the saga in §7; remove the core DB path/open. | Daemon audit shows no core SQLite open; protocol tests cover all reconciliation operations. |
| V03 | Import exists only in `opencrab-gateway-migrate`. | Remove runtime field/import code after offline import, verification, and marker design are implemented. | Daemon config rejects both legacy/core DB fields; production daemon binary has no import symbols; rerun/conflict migration tests pass. |
| V04 | `discord-gatewayd` is canonical owner and supervisor; existing executable becomes its child. | Add Discord store/admin/reconciliation, import existing placement/config, then launch child only from verified state. | Discord can be administered and restarted with server stopped; no operator-authored placement is canonical. |
| V05 | Shared supervisor is platform-neutral and daemon-owned. | Rename vocabulary/API without changing proven backoff/reap semantics; both daemons consume it or equivalent local utility. | Shared-source audit finds no platform names; server has no supervisor/spawn edge to a concrete daemon. |
| V06 | Concrete policy/identity/subscription rows move to gateway stores; core retains generic rows only. | Offline copy/verify, provision generic rows, mark, guarded destructive migration, then delete production schema/query modules. | Fresh and upgraded core schemas lack removed objects and retained-data digests match. |
| V07 | Concrete administration moves from public server routes to gateway-local UDS. | Cut clients over after verified import, then delete concrete DTO/handler/router entries. | Public server returns 404 for concrete gateway admin routes; local daemon admin authorization tests pass. |
| V08 | Server owns no concrete gateway lifecycle; each daemon owns its children. | Delete `AgentGatewayLifecycle`/server registry after external daemons cover live routing; retain only generic extgate liveness. | Server starts and serves generic core APIs with no concrete daemon present and contains no concrete spawn/config/lifecycle path. |
| V09 | Core schedules generic session turns; runtime delivery resolves only canonical generic binding/session IDs. Platform subscriptions and destinations remain gateway-owned. | Replace platform-shaped fire fields/descriptors with binding-based generic envelopes; preserve one core session-lock/subtask/schedule/heartbeat implementation. | A synthetic new kind receives timed fire without shared source change; no shared parser knows a platform prefix; duplicate-turn/lock tests pass. |
| V10 | Every gateway operation declaration requires `authorization`, `dispatch`, `sub_engine`, `sharing`, and `effect` metadata. | Version hello declaration; migrate Discord/Nostr declarations; remove gateway-name fallback/allowlists after compatibility-free cutover. | Arbitrary valid new operation names project, authorize, and dispatch from metadata alone; missing/unknown metadata fails hello; only collision with a built-in name is rejected. |
| V11 | Expand static audits from core-only identifiers to all shared/server production AST and macro/manifests. | Classify allowed historical/test/comment occurrences separately; permit no production platform vocabulary. | A fixture adding a kind branch or platform schema/query/route symbol fails CI. |
| V12 | Web remains independently deployed and owns Web authentication/policy/display state. | Keep current runtime client; move any future durable Web concrete config into a Web-owned store/admin plane before adding it. | Web can be built/deployed without core schema/source changes; no Web policy column/route appears in core/server. |
| V13 | CLI remains an operator-launched independent generic-runtime client. | Keep current path and opaque placement; do not add CLI-specific core policy. | CLI builds/runs against the generic protocol with no core/shared/server change. |
| V14 | QC concrete dependencies remain dev-only. | Preserve harnesses while production audits use `--edges no-dev`; separately assert no dev dependency is promoted. | Cargo metadata proves the concrete crates occur only on dev edges and QC still runs. |
| V15 | Cleanup is semantic, not a repository-wide word deletion. | Use reachability and migration classification; preserve byte-identical histories and required old migrations/fixtures; delete obsolete reachable code deliberately. | Migration preservation checks pass; audit report lists every retained historical occurrence and why it cannot affect production behavior. |
| V16 | New-gateway extension has no shared/server change point. | Remove static lifecycle/fire descriptors, platform API/schema, and name-policy branches; enforce the litmus test in review and CI. | A synthetic new kind provisions, runs, declares operations, receives timed work, and passes QC without core/shared/server source, schema, migration, or redeployment changes. |

### Dynamic operation policy contract

For protocol version 3, every operation declaration contains these required, digest-covered generic fields in addition to name, description, and schemas:

- `authorization.allowed_callers`: a non-empty, sorted subset of `owner`, `co_agent`, `trusted`, and `guest`; core compares only the gateway-authenticated generic caller classification and fails closed;
- `dispatch`: `inline`, `background`, or `utterance`; `utterance` uses the exactly-once delivery path and is never converted into a background subtask;
- `sub_engine`: `not_exposed`, `blocked`, or `allowed`;
- `sharing`: `agent_bound` or `conversation_bound`;
- `effect`: `read_only`, `state_change`, or `utterance`, with `dispatch=utterance` requiring `effect=utterance`.

Unknown enum values, missing fields, duplicate names, invalid schemas, forbidden combinations, or collisions with a built-in core tool name reject hello. Core may reserve its own built-in names, but it has no gateway-operation allowlist, prefix rule, “known utterance” list, or per-name authorization/dispatch branch. The live declaration snapshot is the sole authority for visibility, authorization, dispatch, sub-engine exposure, sharing, invocation, and callback validation. Thus a new tool name requires only a new gateway declaration.

## 5. Gateway-owned reference records and stable IDs

Core subject identity is an immutable, non-reusable UUID with a permanent tombstone. Deleting an agent or subject never permits that UUID to be reassigned. A gateway instance record contains the generic references required to operate without querying core storage:

- stable core `agent_id` (operator display/correlation only; never an authorization key);
- immutable core `subject_id`;
- gateway-owned display name;
- canonical `instance_id`;
- desired platform configuration and encrypted credentials;
- desired binding records, each containing canonical `binding_id`, canonical `session_id`, and opaque `address`;
- desired generation plus last verified core revision/config digest/binding inventory.

The initial records are populated by the offline migration. After cutover, core agent creation or a generic operator export atomically emits `(agent_id, subject_id, subject_grant)`. `subject_grant` is a random, single-use, short-lived capability stored hashed in core and bound to that exact pair. First `PUT instance` must present it; core atomically consumes it and makes the instance-to-subject association immutable. A retry of the byte-identical associated instance succeeds without another grant; any attempt to reuse a grant, change either ID, attach a tombstoned subject, or associate another subject fails closed and is audited. The offline migrator performs the same association through its guarded transaction and records it in the manifest. Core never provides agent-name lookup to a gateway, and an unvalidated copied `(agent_id, subject_id)` pair is never accepted.

New IDs use UUIDv5 with committed, per-gateway namespace UUID constants:

- instance name bytes: `instance\0` followed by UTF-8 `agent_id`;
- session name bytes: `session\0` followed by the canonical external conversation locator;
- binding name bytes: `binding\0` followed by canonical `instance_id`, `\0`, and canonical `session_id`.

The concrete gateway defines and tests its external locator canonicalization. IDs are materialized in its store and are never recalculated after creation. Migration preserves an existing core instance/binding/session ID when one exists, even if an older algorithm produced it. Renaming a display label never changes an ID. A conflicting UUID or locator is a hard migration/provisioning error.

## 6. Generic gate-admin protocol

Gate admin is HTTP/1.1 over the dedicated core gate-admin UDS. Authentication is `Authorization: Bearer`; bodies and errors never echo the token, socket path, SQL, or config bytes. Requests and responses are generic JSON. Error codes are stable generic codes.

The protocol retains six mutating/read operations:

1. `GET /api/gate-instances/{instance_id}`
2. `PUT /api/gate-instances/{instance_id}`
3. `DELETE /api/gate-instances/{instance_id}`
4. `POST /api/gate-instances/{instance_id}/revisions`
5. `PUT /api/gate-bindings/{binding_id}`
6. `DELETE /api/gate-bindings/{binding_id}`

`GET instance` is the discovery operation. Its response includes the instance fields and a transactionally consistent, binding-ID-sorted `bindings` array of all open bindings. Each entry contains only `binding_id`, `address`, and `session_id`. Closed bindings are omitted. This makes revision and binding reconciliation possible without a list-by-kind operation or DB access.

`PUT instance` is byte-idempotent. A repeated identical request returns the stored object; any difference conflicts. First association requires the single-use subject grant from §5; the subject link is then immutable.

`POST revision` requires the current revision and a non-live instance. It atomically changes only enabled/config bytes/digest and increments revision. Stale revision or a live instance conflicts.

`PUT binding` contains `instance_id`, opaque `address`, and a generic session envelope:

```json
{
  "instance_id": "uuid",
  "address": "opaque non-empty string",
  "session": {
    "session_id": "stable id",
    "title": "opaque display title"
  }
}
```

In one core transaction it:

1. verifies an undeleted instance and its immutable subject association; stopped or disabled instances may be provisioned;
2. creates the session and subject membership if the session is absent;
3. otherwise verifies exact session title and subject membership;
4. creates the binding, or returns the existing byte-identical binding;
5. rejects ID conflicts, address reuse, or cross-subject membership.

Core stores the title as ordinary conversation metadata and does not parse it. `CoreBindingService` is the sole creation authority and cannot update an existing session; the scoped admin and runtime entry points below both delegate to it.

`DELETE binding` closes one binding idempotently. Both admin `PUT binding` and runtime lazy `create_binding` call the same internal `CoreBindingService` transaction above: admin is used for daemon desired-state reconciliation; runtime is allowed only for an authenticated live instance discovering an external conversation. Before runtime creation, the child must send the discovery over its private daemon control channel, the daemon must commit it to the desired generation, and only then acknowledge the child. A crash therefore leaves either no binding or a locally desired binding that reconciliation can idempotently create. Neither path can create for another instance or update a session. `DELETE instance` requires it to be non-live and atomically tombstones it and closes all open bindings. No API lists by kind, decodes config, resolves an agent name, or exposes platform vocabulary.

Runtime hello, unlike provisioning, requires an enabled, undeleted instance with matching revision/digest. It declares operation capabilities dynamically using generic operation names, schemas, authorization, dispatch, sharing, sub-engine, and effect metadata defined in §4, plus required `final_delivery` (`automatic` or `operation_driven`). For `automatic`, core emits the normalized final response over the generic delivery frame; for `operation_driven`, no implicit final text is emitted and declared utterance operations perform delivery. Core uses this live metadata and never decodes opaque config to choose final-delivery behavior.

Inbound events carry only the binding, external event ID as opaque dedup material, normalized content/attachments, and a gateway-authenticated generic caller classification (`owner`, `co_agent`, `trusted`, or `guest`). A `co_agent` classification must include internal `agent_id` and relationship revision. Core queries the current internal relationship on every tool execution and turn continuation, requires the reference and revision to match, and rejects immediately after revocation; the gateway remains sole owner of external-ID authentication/mapping. Other external identity/policy data never reaches core. This dynamic contract, together with opaque placement, is what makes the new-gateway litmus test enforceable.

Before approving any protocol or schema change, reviewers must apply the litmus test. A change that requires core to know a new kind, locator syntax, role, event meaning, operation name, display convention, or credential format is invalid even when represented as a nominally generic string column.

## 7. Lifecycle and reconciliation saga

There is no distributed transaction between a gateway database and core. Each gateway store has an explicit saga state:

- `disabled`: no child may run;
- `pending`: desired generation committed locally but not verified in core;
- `provisioning`: child stopped while idempotent core operations run;
- `ready`: exact core revision, digest, and open binding inventory verified;
- `running`: the child for the verified generation is live;
- `error`: reconciliation failed; child is stopped and the stored generic error code is operator-visible.

All gateway admin changes first commit a new desired generation and `pending` state in one local transaction. The sole reconciliation path is:

1. stop and fully reap the existing child;
2. mark `provisioning`;
3. `GET instance`;
4. create it if absent, accept it if byte-identical, or revise it from the observed revision if config/enabled differs;
5. idempotently put every desired binding;
6. delete obsolete bindings only after all desired bindings exist;
7. `GET instance` again and compare revision, digest, enabled value, and the complete binding inventory;
8. commit the applied generation and `ready` in one local transaction;
9. materialize a non-secret placement and start the child;
10. mark `running` only after the child completes runtime hello/bind acknowledgement.

A failure at any step records `error`, leaves the child stopped, and retries from the observed core state. If revision succeeded but a binding failed, retry does not add another revision when the digest already matches. If local verification commit fails after core success, retry rediscovers the exact state. A child is never started from `pending`, `provisioning`, or `error`.

Daemon and child also have a private inherited control channel, distinct from both core UDS paths. The child reports `started`, runtime hello/bind readiness, discovered-binding requests, structured fatal exit reason, and graceful-stop acknowledgement. The daemon persists PID/start nonce, desired generation, consecutive failures, next retry time, and last exit before changing state. `running` requires matching nonce/generation plus hello and all bind acknowledgements. On daemon restart, any stored `running` row is `recovering`: it verifies process identity and core liveness, adopts only an exact nonce/generation match, otherwise terminates/reaps a stale child and reconciles from `pending`. Unexpected exit atomically records stopped/error state and exponential backoff with bounded jitter; stable uptime resets the counter. An operator disable/delete cancels backoff and can never be undone by a stale timer.

Disabling is stop/reap, revision to disabled, close bindings if requested by operator policy, verify, then local commit. Deletion is stop/reap, close bindings, delete instance, verify `instance_unknown`, then delete local non-secret configuration; credentials require a separate explicit destructive confirmation.

Core owns runtime connection liveness but never starts a process. A daemon owns child startup, restart, backoff, placement generation, secret injection, and shutdown. Two daemon instances contending for one gateway database are prevented with an exclusive process lock.

## 8. Secret acquisition

A core gate-admin bearer token is supplied to a daemon from exactly one of:

- a root/operator-created mode-`0600` file; or
- a one-shot environment variable removed immediately after startup.

The token is retained only in a redacted, zeroizing in-memory type. It is not accepted in daemon JSON, gateway rows, placement files, command arguments, logs, metrics, HTTP responses, or errors. Each token identifies a core-owned gate-admin principal and is stored only as a salted hash. Its scope is an explicit set of generic operations plus immutable subject IDs and instance IDs (or a single operator-approved instance-creation namespace); it is never a kind-name wildcard. Every mutation records principal ID, request ID, target IDs, outcome, and timestamp in a generic audit ledger without body/config/token bytes. Rotation creates a new principal credential, allows an explicit bounded overlap, then revokes the old hash; revocation is checked on every request and is immediate. Revoked/expired/out-of-scope tokens return the same sanitized unauthorized error. Only the core operator plane can issue, scope, rotate, or revoke these principals.

Platform credentials are encrypted in the owning gateway database with authenticated encryption. The gateway master key is independently obtained from a mode-`0600` file or a one-shot scrubbed environment variable and is never shared with core. Plaintext exists only in gateway memory and a child's scrubbed inherited environment/pipe for the shortest startup interval. Updating a credential follows the same stop/provision/verify/start saga even when opaque core config is unchanged.

## 9. Offline migration and completeness proof

`opencrab-gateway-migrate` is a separate, offline executable. Daemons do not link it and contain no import code. It requires all services stopped and explicit absolute paths for:

- source core SQLite opened read-only;
- destination Discord SQLite opened read-write;
- destination Nostr SQLite opened read-write;
- backup directory;
- gateway master-key sources needed to encrypt imported credentials;
- output verification manifest.

The tool refuses symlinks, non-regular database files, an incomplete backup, a changing source fingerprint, or non-empty conflicting destination rows. Before copying it creates SQLite-consistent backups of source and destinations. Import is rerunnable: byte-identical rows are accepted and any non-identical row with the same stable key fails.

It copies all concrete Discord/Nostr settings, external identity projections, watches, credentials, stable generic references, and desired placements. Internal co-agent relationships remain in core; their external identity projections are copied to each gateway. Histories, agents, subjects, sessions, generic gate rows, and exactly-once ledgers are not moved or rewritten.

Discord mapping is field-specific: `channel_config.channel_id/agent_id/guild_id/channel_name/readable/writable/whitelisted` becomes the Discord store endpoint plus admission/read/write policy. For a channel, the existing non-empty exact `agent_id` row retains precedence over the `agent_id=''` global fallback; exact rows map to that instance policy scope and global rows map to an explicit gateway-wide fallback scope. The migrator copies and verifies both classes separately and never collapses a global/per-agent pair; `heartbeat_enabled/heartbeat_interval_secs/heartbeat_instructions` becomes core generic `session_heartbeat_config` for the already-bound session and is not copied into Discord; application/bot-user/display/delivery/reaction fields become the Discord instance profile; `trusted_users` rows whose platform is Discord become owner/trusted external projections; external co-agent IDs become gateway projections pointing at the unchanged internal co-agent relationship ID/revision; token/credential columns become encrypted credential records; existing generic instance/binding IDs and opaque addresses are preserved. Nostr config, watches, relay/filter rows, public keys, role projections, and secret keys map analogously to the named Nostr store tables/classes.

Credential import has no silent fallback precedence. For each instance the migrator inventories every legacy DB, file, environment/operator, and existing destination candidate. Zero candidates fails; multiple non-empty candidates must decrypt/normalize to identical bytes or require an explicit per-instance `--credential-source` recorded by source fingerprint in the secret-free manifest. An existing destination credential wins only when its digest matches the selected source. Runtime admin exposes distinct `set`, `rotate`, and confirmed `destroy` operations; update never falls back to old core/TOML data, and plaintext is never returned.

The tool then verifies:

- source and imported counts by data class and gateway instance;
- canonical sorted-row SHA-256 digests by data class and instance;
- every imported credential decrypts inside the corresponding gateway store library without printing plaintext;
- every subject/session reference exists and has the expected membership;
- each desired instance/config digest/binding inventory is equivalent to existing generic placement;
- every gateway row in mixed legacy identity storage is represented once in the owning gateway;
- no destination conflict or unmapped source row remains.

The non-secret manifest records tool/schema versions, source backup SHA-256, per-class counts/digests, per-instance placement digests, destination schema versions, verification time, and an overall manifest digest. It contains no credential, external identity, config bytes, DB path, or socket path.

After isolated QC, all core/gateway processes are stopped again and a post-QC freeze set is created: SQLite-consistent core, Discord, and Nostr snapshots plus their file SHA-256, logical canonical-row digests, schema versions, and a shared freeze ID. `verify-and-mark` verifies the frozen snapshots and the unchanged live files, then writes to core's generic `separation_migrations` table the freeze ID, the core logical digest excluding that marker row, both destination digests/versions, migration version, and manifest digest. Destructive cleanup rechecks those exact logical digests, allowing only the marker row itself to differ. Thus the authorization lineage is post-import and post-QC, not the pre-import backup hash. The marker is the sole authorization for destructive cleanup, is never produced by a daemon, and cannot be bypassed with a runtime flag. The exact rollback set after QC is the three post-QC frozen snapshots, manifest, binaries, and configuration/key-source versions recorded by the freeze ID; pre-import backups are used only for rollback before QC acceptance.

## 10. Cutover and rollback

The only supported cutover order is:

1. stop core and both gateway daemons; verify no gateway child remains;
2. create and verify backups of core and both gateway databases;
3. run offline import and completeness verification;
4. start core with the dedicated gate-admin UDS but keep gateway children stopped;
5. run each daemon in provision-only mode, execute the saga through `ready`, and verify generic GET snapshots;
6. start children and perform isolated inbound, outbound, role-classification, history, and duplicate-event exactly-once checks;
7. stop all services again, create the three-file post-QC freeze set, and run `verify-and-mark`;
8. apply the guarded destructive core migration against that freeze lineage;
9. deploy server/shared binaries with concrete APIs and queries removed;
10. start core, then daemons, and repeat isolated checks.

There is no dual-run, shadow read, feature flag, runtime import, or fallback.

Before QC acceptance, rollback uses the three matched pre-import backups and prior binaries/config. After the post-QC freeze (including after step 8), rollback means stop all processes and restore the exact three post-QC snapshots plus the binaries, configuration, and key-source versions named by the same freeze ID. Reverse reconstruction from opaque config is forbidden.

## 11. Legacy deletion criteria

The destructive migration runs only when the marker freeze ID matches the post-QC frozen set and the live logical core digest (excluding the marker row), both gateway destination digests/schema versions, manifest digest, and migration version still match the recorded values. Otherwise startup/migration fails closed.

In one transaction it:

- removes gateway rows from mixed `trusted_users` storage;
- migrates genuine non-gateway REST/API identities to `api_principals` with no platform column;
- drops `channel_config` only after its generic heartbeat fields have been mapped to `session_heartbeat_config`; drops platform `session_watches`, `agent_discord_config`, `agent_nostr_config`, their indexes/triggers, and obsolete secret columns;
- removes old mixed identity tables after the non-gateway rows are verified;
- preserves agents, subjects, sessions, membership, histories, internal co-agent relationships, opaque gate instances/bindings, and exactly-once/delivery ledgers;
- records the applied generic separation migration.

Fresh schema initialization omits all removed objects. Production `db` query modules for those objects and server channel/trusted-gateway routes are deleted in the same release. Server may expose generic agent/subject/session administration and non-gateway `api_principals`; it may not proxy a gateway admin API.

### Timed turns, schedules, and subtask delivery across disconnects

Core owns schedule/heartbeat rows, fire and subtask ledgers, session locks, and completion state. Each due event receives a stable `fire_id`; each external emission receives a stable `delivery_id` scoped to `binding_id`. Core commits pending work before enqueue, acquires the same session lock as inbound turns, and marks completion only after the gateway acknowledgement is durably recorded. Disconnect leaves work pending; reconnect of the same open binding drains it in ledger order. Repeated frames/acks are idempotent, and a closed/tombstoned binding becomes a terminal non-delivery rather than rerouting by platform prefix.

A subtask captures only `session_id`, `binding_id`, opaque reply target, caller snapshot, and delivery ID. On each continuation core revalidates current co-agent authority as described in §6. An opaque reply target is usable only with its captured binding; core never parses or reconstructs it. If that binding is temporarily offline the completion remains pending, if it is closed the completion records a terminal generic error, and it never falls back to another gateway/session. Schedule/heartbeat fire, subtask completion, automatic final delivery, and utterance operations all use the same exactly-once delivery ledger; retries after daemon/core restart cannot create a second external emission. Platform watches/subscriptions merely create inbound events and never become core schedules.

## 12. Enforcement and release gate

CI scans production Rust and manifests, excluding historical SQL migration fixtures and tests, and fails on:

- concrete gateway crate dependencies from shared/server crates;
- `opencrab-db` or core SQLite dependencies in concrete daemon production targets;
- daemon fields or code for core/legacy database paths or runtime imports;
- production shared/server schema, query, route, DTO, or branch vocabulary for concrete platform settings/identities/secrets;
- gate-admin exposure on public TCP HTTP;
- config decoding or kind enumeration in core;
- plaintext secret fields in opaque config or placement files.

Protocol tests prove byte-idempotent create, conflicting create, stopped-only revision, stable stale-revision errors, atomic generic session/binding creation, duplicate-address rejection, complete sorted binding discovery, sanitized errors, and token redaction. Daemon tests prove role classification, deterministic/preserved IDs, stop-before-change, partial-failure stopped state, retry convergence, and secret encryption/redaction. Migration tests cover populated upgrades, idempotent reruns, conflicting destinations, missing mappings, completeness digests, marker refusal, fresh initialization, preservation of retained data, and matched-backup rollback.

Release is blocked until isolated QC demonstrates existing agent/binding continuity, owner/co-agent/trusted semantics, history preservation, inbound/outbound operation, and duplicate-event exactly-once behavior for both gateways. Production deployment is an operator action outside repository implementation work.

### Design-review completion criteria

Architecture review remains blocked until all of the following are true in the documents: this file, `design-plugin-architecture.md`, and `DESIGN.md` use the same **generic conversation/execution state** versus **concrete platform state/lifecycle** vocabulary and the same target dependency diagram; none says that all plugin configuration belongs in core, that a transport is storage/policy-free, or that `server` owns/supervises a concrete gateway; every V01–V16 item has exactly one transition and measurable completion criterion; and the Issue #1006 acceptance bullets and new-gateway litmus test are all represented. Implementation review remains separately blocked until the mapped evidence is produced.
