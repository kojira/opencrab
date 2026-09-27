# D-1033: Remove `gate_admin` and restore ordinary gateway lifecycle

## Status

Proposed. This document is the implementation design for [#1033](https://github.com/kojira/opencrab/issues/1033). It supersedes the CI-only unblocking work in #1034; that PR did not remove the subsystem.

## User-visible goal

An ordinary configured Discord or Nostr gateway starts and recovers from its durable platform configuration without a separate `gate_admin` socket, credential manifest, principal, or gateway-admin operation. A core with only ordinary `[gate]` configuration starts normally. Existing ordinary Web, Discord, and Nostr flows keep their intended behavior.

## Basis

Before `gate_admin` was introduced, the established lifecycle used platform configuration rows as durable desired state:

```text
platform configuration API
  -> agent_discord_config / agent_nostr_config
  -> AgentGatewayRegistry start/stop
  -> concrete platform manager reads its configuration
  -> live platform connection

server startup
  -> register each platform manager
  -> restore enabled configurations in registration order
```

The historical reference is commit `f684e04` (`7aacd80^`). This is a restoration of that known lifecycle, not a new generic administration protocol.

## Scope

### Remove

- Required `[gate_admin]` configuration, sample configuration, bootstrap credential manifest, private UDS listener, router, bootstrap security/audit code, and its six operations.
- `GateAdminClient`, UDS reconciliation state, and daemon fields that require the private socket/credential.
- Gate-admin-only tests, static-audit rules, and QC harness seeding.

### Restore

- `AgentGatewayLifecycle` / `AgentGatewayRegistry` as the platform-neutral start/stop and ordered boot-recovery seam.
- Server registration and `restore_pending()` at the historical Discord and Nostr boot positions.
- Platform managers and API handlers whose durable authority is `agent_discord_config` or `agent_nostr_config`.
- Existing start/stop ordering: Discord enables then starts; Nostr writes disabled, validates/starts, then enables only after success.

### Preserve

- `opencrab-gate-client` ordinary wire/client functionality; remove only its admin submodule.
- Existing gateway-specific stores only where still needed by ordinary runtime behavior; do not leave them as a second lifecycle authority.
- Historical schema migration v53 and its catalog position. The tables become inert; no destructive table-drop migration is part of #1033.
- #1029/#1031 Nostr delivery semantics. In particular, do not change `delivery_mode`, `final_delivery`, Nostr admission, Nostr watch policy, `provision.rs`, or `adapter.rs` as part of this work.

## Non-goals

- No new replacement gateway-admin security mechanism, public mutation API, or permission model.
- No schema renumbering or removal of prior migrations.
- No production deployment, configuration mutation, or live QC without separate authorization.

## Authority and state

| Platform | Durable lifecycle authority | Runtime-only state | Recovery |
|---|---|---|---|
| Discord | `agent_discord_config` (credential, owner, enabled) | manager task/handle and authenticated bot identity projection | enumerate enabled rows and start each manager |
| Nostr | `agent_nostr_config` (secret, relays/filter, enabled) | manager task/handle and trusted identity projections | enumerate enabled rows and start each manager |

The API must not write gateway-authenticated identity projections. Platform managers retain that responsibility after authenticating to their platform. The registry owns neither credentials nor platform-specific configuration; it only dispatches start/stop and ordered restore calls.

## Lifecycle and failures

- A missing platform configuration, disabled configuration, blank Discord token, or blank Nostr secret fails closed at the platform manager start seam.
- Restart recovery starts only enabled configurations.
- Nostr update/start must retain disabled -> start/validate -> enabled ordering. A start failure leaves it disabled.
- A failure restoring one platform must not silently turn into a gate-admin fallback or a second writer. Existing registry failure behavior is preserved and tested.
- Core startup no longer reads a gate-admin manifest, binds a gate-admin UDS, or serves gate-admin routes.

## Compatibility and migration

Existing config files may still contain a now-unused `[gate_admin]` table; it is not required for startup. Existing v53 schema entries remain so a database at its current schema version is accepted. The removed tables are not read or written by runtime code.

The old daemon reconciliation state is not retained as an optional fallback. Keeping both the daemon store and restored `agent_*_config` lifecycle as writers would create an ambiguous desired-state authority and startup races.

## Implementation checklist

| Design clause | Production seam | Assertion-level RED | Minimal GREEN | Evidence | Prohibited |
|---|---|---|---|---|---|
| Core does not require `gate_admin` | `server` config/bootstrap/main | minimal `[gate]` config fails only because `[gate_admin]` is absent | remove gate-admin parsing/bootstrap/UDS spawn | focused core-start test and Web process suites | optional gate-admin fallback or replacement socket |
| No gate-admin operation surface remains | `extgate` admin/security/socket modules and `gate-client::admin` | compile/static references identify removed surface | delete modules/callers and retire dedicated tests | focused extgate/gate-client tests and route inventory | public replacement routes |
| Discord normal lifecycle is restored | server API + registry + Discord manager + `agent_discord_config` | enabled config cannot be restored without daemon reconciliation | API start/stop and boot restore invoke manager through registry | focused lifecycle/config tests and cold-start fixture | second daemon-store authority |
| Nostr normal lifecycle is restored | server API + registry + Nostr manager + `agent_nostr_config` | failed Nostr start can enable config or boot cannot restore an enabled row | restore historical ordering and manager recovery | focused lifecycle/config tests; existing Nostr delivery tests unchanged | edits to delivery/admission semantics |
| Existing DBs remain compatible | DB migration catalog | current-schema DB rejected after removal | preserve v53 ordering; retire only subsystem-specific assertions | migration/current-schema open test | deleting or renumbering v53 |
| Obsolete CI/QC debt is removed | audits, harnesses, ignored Web suites | tests still seed/assert gate-admin or ordinary Web suites remain ignored | remove only subsystem-specific tests/rules; re-enable ordinary user-flow suites | focused audit, Web suites | weakening unrelated ownership/security assertions |

## Acceptance

1. A core configured without `[gate_admin]` starts and exposes its ordinary gateway runtime.
2. No gate-admin credential manifest or private admin UDS is read, created, or served.
3. An enabled Discord configuration is restored through the registry/manager path after core start.
4. An enabled Nostr configuration is restored through the registry/manager path after core start, with the existing Nostr delivery tests unchanged.
5. A failed Nostr start does not enable its durable configuration.
6. Ordinary Web conversation/reconnect/concurrency/unauthorized process suites are re-enabled and pass without adding gate-admin configuration.
7. Current-schema databases remain openable; v53 is still in migration order.
8. Gateway boundary audit remains strict for all unrelated rules after gate-admin-specific rules are retired.

## Validation order

1. Focused RED/GREEN tests for registry, platform configuration, and bootstrap.
2. Focused server, Discord, Nostr, extgate, gate-client, migration, and boundary-audit tests.
3. Re-enabled Web process suites.
4. Independent code review against this document.
5. Repository CI on a dedicated implementation PR.

No live Discord/Nostr QC or deployment is included in this implementation approval.

## Independent review — blocked

The historical-lifecycle proposal cannot be implemented unchanged:

1. Current-schema fresh databases deliberately lack `agent_discord_config` and `agent_nostr_config`. Reinstating them requires an appended compatibility migration and fresh-schema change; preserving only v53 is insufficient.
2. Discord/Nostr gateway stores and local admin surfaces still persist lifecycle/configuration state. They must be deleted or made runtime-only before the restored core rows can be the sole lifecycle authority; otherwise there are dual writers.
3. Most importantly, returning Nostr lifecycle/configuration authority to core conflicts with the approved #1006 separation, where gateway-owned Nostr runtime/admission state is deliberately not a core projection. The historical path predates that boundary.

Implementation is blocked pending a user decision on the intended post-#1006 ownership model. This review does not add requirements beyond resolving that contradiction.
