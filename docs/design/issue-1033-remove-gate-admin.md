# D-1033: Remove `gate_admin` with gateway self-provisioning

## Status

Proposed implementation design for [#1033](https://github.com/kojira/opencrab/issues/1033). #1034 only unblocked CI; it did not remove the subsystem.

## Goal

Remove the separate core `gate_admin` subsystem—its required configuration, bootstrap credential, principal/audit tables at runtime, private admin UDS, HTTP routes, and client—without breaking ordinary gateway administration or the #1006 ownership boundary.

A gateway remains administered through its existing gateway-local scoped admin UDS. Its local store is the sole desired-state authority. The core retains only the generic instance, revision, and binding projection required by V3 runtime traffic.

## User-visible behavior

- A core configured with ordinary `[gate]` but no `[gate_admin]` starts normally.
- The existing gateway-local mode-0600 scoped admin UDS continues to accept configuration/lifecycle changes for its configured instance IDs. It retains its current exact target/scope validation and never returns stored credentials.
- After a gateway-local desired update, the gateway makes its generic projection available to core, then connects with unchanged V3 `hello`/`bind`/`said`/`say` runtime traffic.
- Existing platform owner classification continues to govern normal Discord/Nostr gateway interactions. This work does not invent a chat-based configuration interface.

## Ownership and security boundary

```text
gateway-local scoped admin UDS
  -> gateway-owned desired store
  -> one generic self-provision request on existing core V3 UDS
  -> core generic instance/revision/binding projection
  -> unchanged V3 hello/bind/said/say runtime connection
```

| State | Owner | Constraint |
|---|---|---|
| Discord/Nostr credentials, runtime configuration, admission, watches, delivery | concrete gateway and its local store | never serialized into core provisioning data |
| Gateway desired enabled state and lifecycle generation | concrete gateway and its local store | sole desired-state authority |
| Generic instance identity, opaque config digest/revision, generic bindings | core extgate | projection only; no platform-specific interpretation |

The existing ordinary core V3 UDS file mode/group is the access boundary for the gateway process. The new message carries no bearer, credential, principal, HTTP route, platform name, secret, admission rule, watch, or delivery field. Its optional single-use `subject_grant` is the existing v54 subject-association grant: it is encrypted at rest in the gateway-local store, supplied only to create a new generic instance, and never becomes a V3 login credential. It is not a second management transport: it is a one-shot declaration on the already-required V3 transport. Gateway-local administration remains target-scoped; the core does not accept user-originated configuration requests.

## V3 protocol extension

Before ordinary `hello`, a gateway may send exactly one `provision` frame and receive one `provisioned` frame. This is a defined V3 request/response extension using the existing `m` and request-ID grammar:

```json
{
  "m": "provision",
  "id": "request-id",
  "instance_id": "UUID",
  "kind_id": "opaque nonempty string",
  "subject_id": 1,
  "subject_grant": "optional existing single-use v54 association grant",
  "adopt_existing": false,
  "enabled": true,
  "config_b64": "base64 generic-only config",
  "bindings": [{"binding_id": "UUID", "address": "opaque nonempty string"}]
}
```

A successful response is exactly:

```json
{
  "m": "provisioned",
  "id": "same request-id",
  "instance_id": "UUID",
  "revision": 1,
  "config_digest": "sha256 hex",
  "enabled": true,
  "bindings": [{"binding_id": "UUID", "address": "opaque string"}]
}
```

`m`, `id`, `instance_id`, `kind_id`, `subject_id`, `enabled`, `config_b64`, and `bindings` are required. `subject_grant` and `adopt_existing` are optional; omitted `adopt_existing` is `false`. No other field is accepted. The parser rejects duplicate keys, unknown fields, wrong types, empty strings, invalid base64, non-canonical IDs, nonpositive `subject_id`, or duplicate binding ID/address with `bad_request`.

Failure behavior is exact: unknown `subject_id` -> `subject_unknown`; a missing/invalid/expired/consumed/mismatched association grant -> `instance_conflict`; deleted/kind/subject mismatch or an existing `runtime` instance without `adopt_existing=true` -> `instance_conflict`; live instance revision/config/enabled/inventory change -> `instance_active`; a binding ID/session mismatch -> `binding_conflict`; an address owned by another instance -> `address_in_use`; storage failure -> `store_error`. Every parseable-ID failure emits that existing `err` frame then closes; an unparseable/missing request ID closes without a reply. Success writes `provisioned` and closes. The only valid PreHello transitions are `provision -> closed` and `hello -> Running`; every other pre-hello frame, and every `provision` after hello, emits `protocol_order` then closes. `hello` fields and behavior are unchanged.

The gateway derives stable binding IDs using the existing UUID-v5 `(instance_id, address)` convention. Core derives each session exactly as the previous reconciler did: `session_id = "extgate-{binding_id}"`, title = `"gateway session"`, and the instance subject is its sole member. `bindings` is a complete inventory for a **declarative** instance, not a patch; an exact retry must preserve the existing session/title/membership rather than recreate it.

Core processes a valid frame in one immediate transaction:

1. Validate canonical identifiers, positive `subject_id`, nonempty opaque fields, and unique binding IDs/addresses.
2. Resolve `subject_id` to exactly one existing `agents` row. On create, consume the supplied unexpired v54 grant for that exact `(agent_id, subject_id)` in this same transaction; do not consume a grant for an existing exact instance or an unsuccessful transaction. Gateway-local grant envelopes remain until this success then are cleared by the daemon as today.
3. Create the generic instance when absent with `binding_authority='declarative'`. For a pre-#1033 existing inactive `runtime` instance, accept the one-way generic authority transfer only when `adopt_existing=true` and kind/subject match; atomically set `binding_authority='declarative'` with the requested projection/inventory. No other runtime instance can be provisioned. This lets gateway-local desired rows retain their existing IDs while Web/CLI rows remain runtime unless explicitly transferred.
4. If generic config or enabled state changed, create at most one revision. An identical retry changes nothing.
5. Create/reuse every declared generic binding and close only open bindings for that declarative instance absent from the complete inventory.
6. Reject a binding address owned by another instance, malformed data, or a live-instance change before any mutation.
7. Commit and return the observed instance/revision/digest/binding snapshot in `provisioned`.

`binding_authority` is a generic core column introduced by a forward migration with default `runtime`. Existing Web/CLI instances remain `runtime` and retain the existing post-hello `create_binding` behavior. `create_binding` rejects declarative instances; thus a gateway provision inventory is its only binding writer. An ordinary `hello` never creates or revises state; it continues to verify an enabled pre-existing generic projection and then sends existing `ok` and persisted `bind` frames.

## Gateway lifecycle

`DiscordStore`/`NostrStore` remains the only desired state. Their daemons replace `GateAdminClient`/`UdsGateReconciler` with a V3 self-provision client that serializes only the generic projection already derived from that store. Both daemons must use an exact gateway-side `core_config_b64_for_runtime_config` helper. Nostr retains its current helper, which preserves the configured generic `delivery_mode` compatibility value. Discord gains a helper that validates its local runtime config but **always** emits the generic `{"delivery_mode":"tool_driven"}` projection because its unchanged runtime hello always declares `final_delivery=OperationDriven`; omitted, `say`, and `tool_driven` local settings all use that same core projection. Neither helper may emit Discord access IDs/reactions or Nostr secrets/relays/access/watches; field-by-field tests prove it. A desired change follows the existing safe lifecycle order: stop/disconnect a live child, wait until the core no longer marks it live, provision atomically, then spawn/reconnect the unchanged runtime child. Existing rows use `adopt_existing=true` for their first successful provision; later retries use `false`. Recovery retries the same idempotent declaration.

No historical `agent_discord_config`/`agent_nostr_config` manager path is restored. This avoids moving Nostr runtime or admission authority back into core and avoids a dual writer.

## Remove

- `[gate_admin]` parsing and configuration samples.
- Gate-admin manifest bootstrap, credential/principal/audit runtime logic, private admin UDS, admin router, and startup task.
- `extgate` admin/security/socket modules and registry authentication/target authorization paths.
- `gate-client::admin` HTTP client and its multi-request reconciliation.
- Daemon `gate_admin_socket`/`gate_admin_credential` fields and all gate-admin-specific tests, static audit rules, and QC harness seeding.

The historical v53 migration and its position remain intact so existing databases remain compatible. Its now-unused tables are inert. #1033 adds only the forward generic `binding_authority` migration; it adds no destructive migration or renumbering.

## Preserve and prohibit

Preserve gateway-local scoped admin UDS behavior and non-gate-admin tests. Preserve post-hello `create_binding` only for existing `binding_authority='runtime'` instances used by Web/CLI; it is rejected for declarative gateway instances. Do not add a public configuration route, a new core UDS, bearer token, principal, manifest, platform-specific core field, direct core database writer in a gateway, or a fallback that retains the old admin system.

Do not modify #1029/#1031 behavior: `delivery_mode`, `final_delivery`, Nostr admission, watch policy, `provision.rs`, and `adapter.rs` are outside #1033.

## Assertion checklist

| Design clause | Production seam | Assertion-level RED | Minimal GREEN | Evidence | Prohibited |
|---|---|---|---|---|---|
| No required core gate-admin startup | server config/bootstrap/main | minimal `[gate]` config fails due to absent `[gate_admin]` | delete config/bootstrap/router/task | focused startup and Web process suites | fallback socket/manifest |
| Gateway remains sole desired authority | Discord/Nostr stores and daemons | local desired update requires core admin client | replace only generic reconciliation with self-provision client | daemon recovery tests | core platform config or dual writer |
| Generic projection is atomic/idempotent | V3 parser/listen/registry | second binding failure exposes partial instance/inventory | one immediate transaction and exact retry no-op | V3 socket/conformance tests | multi-request HTTP reconciliation |
| Ordinary runtime stays unchanged | V3 hello/bind and gate client wire | hello may create state or pre-hello unknown frame succeeds | defined `m`/`id` provision closes; later hello verifies and receives bind | wire state-table regression | provisioning through ordinary hello |
| Declarative binding inventory has one writer | generic instance/binding schema and create-binding handler | runtime create_binding mutates a declarative instance | forward `binding_authority` migration; reject only declarative runtime mutation; explicit one-way inactive adoption for existing gateway rows | Web/CLI regression, declarative rejection, upgrade adoption | disable existing Web/CLI binding creation |
| Core owns no platform details | daemon generic projection | Discord/Nostr runtime field appears in provisioned bytes | Nostr preserves its generic compatibility projection; Discord always projects `tool_driven` to match unchanged hello | serialization and hello-compatibility tests | secrets, access, reactions, relays, watches in core |
| Subject association remains valid | v54 grant + provision transaction | new instance binds arbitrary/missing subject or consumes grant on failure | resolve unique agent and consume exact grant only on new committed instance | exact error/create/retry/failure transaction tests | treating a grant as V3 authentication |
| Existing DBs remain compatible | migration catalog | current schema version rejected | retain v53; append generic authority migration | current-schema open/migration test | delete/reorder v53 |
| Obsolete test debt is removed | audits/harnesses/Web suites | tests seed/assert old system or Web remains ignored | remove only old-system tests; re-enable Web suites | focused audit/Web suites | weakening unrelated checks |

## Acceptance

1. Core starts without `[gate_admin]`, credential manifest, admin UDS, or admin HTTP routes.
2. Gateway-local scoped administration remains functional and target-scoped.
3. A gateway-local desired record atomically produces its generic core projection through the specified V3 `m=provision` transaction, with no platform-specific bytes in core.
4. Identical provision retries do not change revision, bindings, sessions, or grant state; malformed/conflicting/live changes emit the specified existing code and leave all generic state unchanged.
5. Existing inactive gateway rows transition once through `adopt_existing`; declarative gateway instances reject post-hello `create_binding`, while existing Web/CLI runtime instances retain it.
6. The normal post-provision `hello` contract is unchanged: only enabled exact revision/digest snapshots receive `ok` then `bind`.
7. Discord/Nostr recovery uses the idempotent path without gate-admin configuration; Nostr generic projection is unchanged, Discord's is hello-compatible for omitted/`say`/`tool_driven`, runtime fields are excluded, and Nostr delivery/admission tests remain unchanged.
8. Ordinary Web conversation/reconnect/concurrency/unauthorized process suites are re-enabled and pass.
9. Current-schema databases still open; v53 remains in order; the forward generic authority migration applies; unrelated boundary rules remain strict.

## Validation order

1. Focused protocol RED/GREEN tests, then extgate/gate-client/daemon recovery tests.
2. Focused core startup, migration, audit, and re-enabled Web process suites.
3. Independent implementation review against this document.
4. Repository CI on the implementation PR.

No deployment or live QC is included in this implementation approval.
