# Gateway process and storage ownership

Status: **authoritative design for Issue #1006; implementation requires design review first**. This decision rejects the prior bidirectional ownership and runtime-import design. There is no compatibility fallback.

This is a design-stage correction, not a rejection of every existing component. The generic runtime UDS framing, opaque `kind_id`/config/address storage, generic caller roles, generic exactly-once processing, and dynamically declared operation capabilities are retained because they satisfy the invariants below. Direct core-SQLite access, runtime legacy import, core-owned concrete settings/identities, server concrete administration, and placement-only Discord ownership are rejected and redesigned.

## 1. Requirements and primary invariant

**New-gateway litmus test:** adding an entirely new concrete gateway requires all of the following:

- zero changes to core, shared, or server source;
- zero core database schema or data migration changes;
- zero core redeployment;
- only a new independently deployed gateway implementation, its gateway-owned schema/admin interface, and generic protocol configuration.

Core sees only opaque `kind_id`, opaque non-secret config bytes, opaque addresses, generic subject/instance/binding/revision identifiers, generic caller roles/events, and dynamically declared operation capabilities. If a future gateway cannot be implemented with the protocol, the remedy is a platform-neutral protocol primitive usable without naming or recognizing that gateway. A concrete core field, table, API, enum variant, prefix branch, or dependency is always rejected.

A concrete gateway is the only owner of its platform configuration, external identities, credentials, authorization projection, policy, display semantics, schema, local administration, and lifecycle. Core treats gateway identifiers and configuration as opaque bytes and never branches on a gateway kind.

```text
operator
  |
  | concrete gateway admin UDS
  v
gateway daemon ---- gateway-owned SQLite ---- encrypted gateway secrets
  |
  | generic gate-admin HTTP over a dedicated UDS
  v
core gate admin ---- core conversation SQLite
  ^
  | generic runtime gate protocol over a different UDS
  |
gateway instance process
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
| internal agent-to-agent trust relationship | core | No external account identifier is stored with it. |
| generic gate instance ID, opaque kind ID, subject ID, revision, enabled flag, opaque non-secret config bytes/digest | core | Core compares/stores values but does not enumerate or decode kind/config. |
| generic binding ID, instance ID, opaque address, session ID, liveness timestamps | core | Core enforces subject/session membership and address uniqueness only. |
| inbound/outbound exactly-once ledgers and delivery state | core | Existing ledgers and keys are preserved through migration. |
| platform endpoint IDs, account/application/bot IDs, names, filters, relays, channel policy, reactions, delivery settings | the corresponding gateway store | Never named columns or interpreted values in core. |
| external owner/co-agent/trusted identities and their generic role projection | the corresponding gateway store | Core receives only a generic authenticated caller role and optional internal agent reference on runtime messages. |
| watches, polling intervals, platform subscription filters | the corresponding gateway store | A gateway may trigger a generic core session; core does not understand the filter. |
| platform credentials and tokens | the corresponding gateway daemon/store | Encrypted at rest; never in opaque core config. |
| gateway child state, backoff, desired/applied generation, reconciliation errors | the corresponding gateway daemon/store | The daemon is the sole lifecycle authority. |
| non-gateway API principals | core `api_principals` | This model has no platform discriminator and cannot contain gateway account IDs. |

### Discord

`discord-gatewayd` owns a Discord SQLite database and a local admin UDS. It owns channel/guild/application/bot-user IDs, channel admission/read/write policy, heartbeat overrides, delivery mode, reactions, owner/co-agent/trusted mappings, bot credentials, and all Discord schema. It supervises one `discord-gateway-instance` child per enabled instance. The current placement-only executable becomes that child; placement files are generated only by `discord-gatewayd` and are not canonical storage.

### Nostr

`nostr-gatewayd` owns a Nostr SQLite database and a local admin UDS. It owns relay/filter settings, watches, public-key mappings, owner/co-agent/trusted mappings, encrypted secret keys, and all Nostr schema. It supervises one `nostr-gateway-instance` child per enabled instance.

Neither daemon accepts a core database path or a legacy database path. Neither daemon opens any database other than its own gateway database.

## 3. Gateway-owned reference records and stable IDs

A gateway instance record contains the generic references required to operate without querying core storage:

- stable core `agent_id` (for operator display/correlation only);
- positive core `subject_id`;
- gateway-owned display name;
- canonical `instance_id`;
- desired platform configuration and encrypted credentials;
- desired binding records, each containing canonical `binding_id`, canonical `session_id`, and opaque `address`;
- desired generation plus last verified core revision/config digest/binding inventory.

The initial records are populated by the offline migration. After cutover, a generic core agent-creation response or operator export supplies `agent_id` and `subject_id`; the operator must pass both explicitly to gateway admin. Core does not provide an agent-name lookup endpoint to a concrete gateway. Missing or conflicting references fail closed.

New IDs use UUIDv5 with committed, per-gateway namespace UUID constants:

- instance name bytes: `instance\0` followed by UTF-8 `agent_id`;
- session name bytes: `session\0` followed by the canonical external conversation locator;
- binding name bytes: `binding\0` followed by canonical `instance_id`, `\0`, and canonical `session_id`.

The concrete gateway defines and tests its external locator canonicalization. IDs are materialized in its store and are never recalculated after creation. Migration preserves an existing core instance/binding/session ID when one exists, even if an older algorithm produced it. Renaming a display label never changes an ID. A conflicting UUID or locator is a hard migration/provisioning error.

## 4. Generic gate-admin protocol

Gate admin is HTTP/1.1 over the dedicated core gate-admin UDS. Authentication is `Authorization: Bearer`; bodies and errors never echo the token, socket path, SQL, or config bytes. Requests and responses are generic JSON. Error codes are stable generic codes.

The protocol retains six mutating/read operations:

1. `GET /api/gate-instances/{instance_id}`
2. `PUT /api/gate-instances/{instance_id}`
3. `DELETE /api/gate-instances/{instance_id}`
4. `POST /api/gate-instances/{instance_id}/revisions`
5. `PUT /api/gate-bindings/{binding_id}`
6. `DELETE /api/gate-bindings/{binding_id}`

`GET instance` is the discovery operation. Its response includes the instance fields and a transactionally consistent, binding-ID-sorted `bindings` array of all open bindings. Each entry contains only `binding_id`, `address`, and `session_id`. Closed bindings are omitted. This makes revision and binding reconciliation possible without a list-by-kind operation or DB access.

`PUT instance` is byte-idempotent. A repeated identical request returns the stored object; any difference conflicts. It requires an existing subject.

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

1. verifies an enabled, undeleted instance and its subject;
2. creates the session and subject membership if the session is absent;
3. otherwise verifies exact session title and subject membership;
4. creates the binding, or returns the existing byte-identical binding;
5. rejects ID conflicts, address reuse, or cross-subject membership.

Core stores the title as ordinary conversation metadata and does not parse it. This operation is the only gateway path that may create a conversation session. It cannot update an existing session.

`DELETE binding` closes one binding idempotently. `DELETE instance` requires it to be non-live and atomically tombstones it and closes all open bindings. No API lists by kind, decodes config, resolves an agent name, or exposes platform vocabulary.

The runtime hello declares operation capabilities dynamically using generic operation names, input schemas, sharing class, and sub-engine metadata. Core validates declaration shape/digest and routes generic invokes; it has no compile-time list of gateway operations. Inbound events carry only the binding, external event id as opaque dedup material, normalized content/attachments, and a gateway-authenticated generic caller classification (`owner`, `co_agent` with optional internal agent reference, `trusted`, or `guest`). Core never receives or re-evaluates the external identity or authorization policy. This dynamic contract, together with opaque placement, is what makes the new-gateway litmus test enforceable.

Before approving any protocol or schema change, reviewers must apply the litmus test. A change that requires core to know a new kind, locator syntax, role, event meaning, operation name, display convention, or credential format is invalid even when represented as a nominally generic string column.

## 5. Lifecycle and reconciliation saga

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

Disabling is stop/reap, revision to disabled, close bindings if requested by operator policy, verify, then local commit. Deletion is stop/reap, close bindings, delete instance, verify `instance_unknown`, then delete local non-secret configuration; credentials require a separate explicit destructive confirmation.

Core owns runtime connection liveness but never starts a process. A daemon owns child startup, restart, backoff, placement generation, secret injection, and shutdown. Two daemon instances contending for one gateway database are prevented with an exclusive process lock.

## 6. Secret acquisition

A core gate-admin bearer token is supplied to a daemon from exactly one of:

- a root/operator-created mode-`0600` file; or
- a one-shot environment variable removed immediately after startup.

The token is retained only in a redacted, zeroizing in-memory type. It is not accepted in daemon JSON, gateway rows, placement files, command arguments, logs, metrics, HTTP responses, or errors.

Platform credentials are encrypted in the owning gateway database with authenticated encryption. The gateway master key is independently obtained from a mode-`0600` file or a one-shot scrubbed environment variable and is never shared with core. Plaintext exists only in gateway memory and a child's scrubbed inherited environment/pipe for the shortest startup interval. Updating a credential follows the same stop/provision/verify/start saga even when opaque core config is unchanged.

## 7. Offline migration and completeness proof

`opencrab-gateway-migrate` is a separate, offline executable. Daemons do not link it and contain no import code. It requires all services stopped and explicit absolute paths for:

- source core SQLite opened read-only;
- destination Discord SQLite opened read-write;
- destination Nostr SQLite opened read-write;
- backup directory;
- gateway master-key sources needed to encrypt imported credentials;
- output verification manifest.

The tool refuses symlinks, non-regular database files, an incomplete backup, a changing source fingerprint, or non-empty conflicting destination rows. Before copying it creates SQLite-consistent backups of source and destinations. Import is rerunnable: byte-identical rows are accepted and any non-identical row with the same stable key fails.

It copies all concrete Discord/Nostr settings, external identity projections, watches, credentials, stable generic references, and desired placements. Internal co-agent relationships remain in core; their external identity projections are copied to each gateway. Histories, agents, subjects, sessions, generic gate rows, and exactly-once ledgers are not moved or rewritten.

The tool then verifies:

- source and imported counts by data class and gateway instance;
- canonical sorted-row SHA-256 digests by data class and instance;
- every imported credential decrypts inside the corresponding gateway store library without printing plaintext;
- every subject/session reference exists and has the expected membership;
- each desired instance/config digest/binding inventory is equivalent to existing generic placement;
- every gateway row in mixed legacy identity storage is represented once in the owning gateway;
- no destination conflict or unmapped source row remains.

The non-secret manifest records tool/schema versions, source backup SHA-256, per-class counts/digests, per-instance placement digests, destination schema versions, verification time, and an overall manifest digest. It contains no credential, external identity, config bytes, DB path, or socket path.

A separate offline `verify-and-mark` invocation repeats verification against unchanged backups and live destination files, then writes the overall digest and required destination schema versions to core's generic `separation_migrations` metadata table in one short transaction. This marker is the sole authorization for destructive core cleanup. It is not produced by a daemon and cannot be bypassed with a runtime flag.

## 8. Cutover and rollback

The only supported cutover order is:

1. stop core and both gateway daemons; verify no gateway child remains;
2. create and verify backups of core and both gateway databases;
3. run offline import and completeness verification;
4. start core with the dedicated gate-admin UDS but keep gateway children stopped;
5. run each daemon in provision-only mode, execute the saga through `ready`, and verify generic GET snapshots;
6. start children and perform isolated inbound, outbound, role-classification, history, and duplicate-event exactly-once checks;
7. stop all services again and run `verify-and-mark`;
8. apply the guarded destructive core migration;
9. deploy server/shared binaries with concrete APIs and queries removed;
10. start core, then daemons, and repeat isolated checks.

There is no dual-run, shadow read, feature flag, runtime import, or fallback.

Before step 8, rollback means stop new daemons, restore all three backups, and run prior binaries. After step 8, rollback means stop all processes and restore all three matched backups plus prior binaries. Reverse reconstruction from opaque config is forbidden.

## 9. Legacy deletion criteria

The destructive migration runs only when the verification marker matches the current core backup fingerprint, expected gateway schema versions, and migration version. Otherwise startup/migration fails closed.

In one transaction it:

- removes gateway rows from mixed `trusted_users` storage;
- migrates genuine non-gateway REST/API identities to `api_principals` with no platform column;
- drops `channel_config`, `session_watches`, `agent_discord_config`, `agent_nostr_config`, their indexes/triggers, and obsolete secret columns;
- removes old mixed identity tables after the non-gateway rows are verified;
- preserves agents, subjects, sessions, membership, histories, internal co-agent relationships, opaque gate instances/bindings, and exactly-once/delivery ledgers;
- records the applied generic separation migration.

Fresh schema initialization omits all removed objects. Production `db` query modules for those objects and server channel/trusted-gateway routes are deleted in the same release. Server may expose generic agent/subject/session administration and non-gateway `api_principals`; it may not proxy a gateway admin API.

## 10. Enforcement and release gate

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
