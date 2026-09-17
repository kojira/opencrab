# Design: extgate binding-address aliases for timed fire

Issue: [#996](https://github.com/kojira/opencrab/issues/996)

Status: implementation-ready

## 1. Problem and decision

A generic extgate binding can reuse a session when `gate_bindings.address` is byte-equal to an existing `sessions.id`. `canonical_session_id` then returns that address, and inbound records history in the reused session. Timed-fire routing does not follow the same rule: `ExtgateFire::parse` accepts only a physical `extgate-<binding UUID>` ID. Heartbeat tools, schedule validation, and scheduler rebuild therefore reject an enabled reused-address session before the extgate sink can run.

The approved decision is to keep the existing canonical session and add a generic, exact binding-address alias lookup. We will not move history or configuration to a physical extgate session.

This is an extgate invariant, not a protocol rule. No shared implementation may recognize a protocol kind, protocol session prefix, protocol-specific type, or protocol topology. Protocol-specific regression setup belongs only to `crates/nostr` or `crates/nostr-gateway`. There is no change under `crates/core`.

## 2. Ownership

| Concern | Owner |
|---|---|
| Canonical session selection and exact session-to-binding lookup | `crates/db/src/queries/gate_binding.rs` |
| Static timed-fire parsing contract and DB-aware persisted-target hook | `crates/actions/src/timed_fire.rs` |
| extgate physical/alias target construction and fire-time revalidation | `crates/extgate/src/fire.rs` |
| Production descriptor registration | Existing `crates/server/src/lib.rs` registration; no protocol-specific registration |
| Tool/API validation and scheduler enumeration | Existing server callers of the timed-fire router |
| Protocol provisioning regression | `crates/nostr/src/gate_provision.rs` (or a test under `crates/nostr-gateway`) |

The DB layer reports generic binding facts. It does not construct a `FireTarget`. Extgate owns the mapping from those facts to `EXTGATE_TIMED_FIRE_KIND`. Server code asks the router and does not inspect an address or gateway kind.

## 3. Current sequence

1. Binding creation calls `create_gate_binding_in_tx`.
2. If `address` already names a session, binding creation preserves that session and does not create `extgate-<binding_id>`.
3. Inbound calls `canonical_session_id(binding_id, address)` and persists into `address`.
4. Heartbeat/schedule configuration stores that same canonical `session_id`.
5. Tools, schedule APIs, and `scheduler::rebuild_entries` call `TimedFireRouter::resolve_target(session_id, agent_id)`.
6. `ExtgateFire::parse` accepts only the physical form, so a reused address returns `None` and is rejected or skipped.

The sink's live-binding checks are never reached.

## 4. Repaired sequence

1. Binding creation and inbound remain unchanged.
2. Every server path that validates a stored/current session uses a new DB-aware router entry point, `resolve_persisted_target(conn, session_id, agent_id)`.
3. The router asks each descriptor to resolve the persisted session. The trait's default implementation delegates to the existing static `parse`, preserving other transports.
4. `ExtgateFire` overrides persisted resolution. It asks the DB query layer for the one open, non-deleted binding whose canonical session is exactly `session_id`.
5. Extgate accepts the result only when its owning `agent_id` exactly equals the requested agent. It returns a target whose `route` is the full canonical session ID.
6. `ExtgateFire::build_session_id` returns that full route. Thus `run_one_heartbeat` places the original canonical session ID, not a synthesized physical ID, into `TimedFireRequest`.
7. `ExtgateTimedFireSink` performs the same canonical-session lookup again, verifies the requested agent again, resolves binding context, and then applies the existing live-instance and acknowledged-binding checks.
8. The existing said-less extgate turn runs against the reused canonical session, preserving history, delivery behavior, locks, continuation, and completion handling.

The second lookup is intentional TOCTOU protection: closing/deleting/replacing a binding between scheduler resolution and sink receipt must stop delivery.

## 5. DB API and exact lookup

Add the following platform-neutral query types to `gate_binding.rs` (names are part of this design):

```rust
pub struct CanonicalGateBinding {
    pub binding_id: String,
    pub agent_id: String,
}

pub enum CanonicalGateBindingLookup {
    Match(CanonicalGateBinding),
    NotFound,
    Ambiguous,
}

pub fn lookup_canonical_gate_binding(
    conn: &Connection,
    session_id: &str,
) -> Result<CanonicalGateBindingLookup>;
```

Algorithm:

1. Build a candidate set from both existing generic extgate representations:
   - a binding whose physical session ID is exactly `session_id`; and
   - every binding whose `address` is byte-equal to `session_id`.
2. Candidates must have `gate_bindings.closed_at IS NULL` and a joined `gate_instances.deleted_at IS NULL`.
3. Join through `gate_instances.subject_id` to `agents.subject_id` to return the binding's owning agent.
4. For every candidate, call the existing `canonical_session_id(binding_id, address)` and retain it only if the result is byte-equal to the input. This prevents an address from being treated as an alias when that binding actually has a physical canonical session.
5. Deduplicate by `binding_id` because the physical and address candidate paths can identify the same row.
6. Return `Match` only for exactly one retained binding, `NotFound` for zero, and `Ambiguous` for more than one. Query/storage errors remain `Err`; callers log and fail closed.

Use separate indexed queries rather than an `OR`: binding ID uses the binding key, and address uses the existing open-address index. This is read-only and needs no schema change.

`enabled` is deliberately not part of identity resolution. A stopped/disabled instance must remain inspectable by heartbeat tools, matching physical-session behavior. Actual firing still requires a live instance and binding acknowledgement in `ExtgateTimedFireSink`.

## 6. Timed-fire API

Keep `TransportFire::parse` and `TimedFireRouter::resolve_target` for static format checks, descriptor collision checks, and existing generic tests. Add a DB-aware hook with a safe default:

```rust
fn resolve_persisted(
    &self,
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
) -> Option<FireTarget> {
    self.parse(session_id, agent_id)
}
```

Add `TimedFireRouter::resolve_persisted_target(conn, session_id, agent_id)`. It calls `resolve_persisted` on all registered descriptors and returns a target only when there is exactly one distinct match. Zero or multiple matches return `None`; multiple matches emit a warning without exposing session content. This makes dynamic alias collisions fail closed rather than depend on registration order.

`ExtgateFire::resolve_persisted` uses `lookup_canonical_gate_binding`, rejects `NotFound`, `Ambiguous`, DB errors, and agent mismatch, and otherwise returns:

```rust
FireTarget {
    kind: EXTGATE_TIMED_FIRE_KIND,
    channel_id: String::new(),
    guild_id: String::new(),
    route: session_id.to_owned(),
}
```

Change the existing physical `parse` target to use the full input session ID in `route`, and make `build_session_id` return `target.route.clone()`. The physical round trip remains byte-identical, while an alias also round-trips without encoding or parsing its address. `sample_target` is updated to contain the full synthetic physical session ID.

No new extgate descriptor, sink kind, protocol branch, or global registry is introduced.

## 7. Agent ownership and permission invariants

The following are mandatory:

- Persisted extgate resolution succeeds only when the unique binding owner equals the requested `agent_id`.
- A different agent gets `None`, even if it knows an exact session ID.
- The sink repeats ownership validation using `TimedFireRequest.agent_id`; it does not trust a previously built target.
- Only open bindings on non-deleted instances resolve.
- Fire-time delivery still requires the instance to be live and its registry entry to contain the binding in `acknowledged`.
- `TimedFireRequest.caller` remains `CallerIdentity::Owner`; alias resolution does not grant a new caller identity.
- Existing session membership checks at binding creation remain unchanged.
- Lookup is exact and case-sensitive. No prefix, substring, normalization, decoding, or protocol-specific fallback is allowed.
- Lookup errors and ambiguity never choose an arbitrary binding and never cause external delivery.

These checks apply equally to every generic extgate binding address. Protocol-specific owner or address interpretation is out of scope.

## 8. Ambiguity and fail-closed rules

| State | Result |
|---|---|
| No canonical binding | no target |
| Binding is closed | no target |
| Instance is deleted | no target |
| Canonical session row is absent | no target |
| Physical session exists, so address is not canonical | address is not an alias |
| Exactly one canonical binding, wrong agent | no target |
| More than one canonical candidate, including corrupt/legacy data | ambiguous; no target |
| DB lock/query failure | warning; no target |
| Binding closes after target resolution | sink revalidation rejects |
| Instance is disconnected or binding is not acknowledged | existing sink warning; no turn |

The physical static parser remains available only for format/collision checks. Product validation and scheduler enumeration use the persisted resolver, so static syntax alone does not authorize a fire.

## 9. Binding lifecycle, reconnect, and idempotency

- **Creation/reuse:** no change. Existing transaction logic chooses physical creation or address reuse.
- **Reconnect:** hello replay and bind acknowledgement remain unchanged. Persistent alias lookup continues to return the same binding while disconnected, but the sink refuses delivery until acknowledgement returns.
- **Close:** a closed binding stops resolving immediately. Existing heartbeat/schedule rows remain untouched and can become routable again only through a valid new open binding for the same canonical address and owner.
- **Delete:** a deleted instance never resolves.
- **Replacement:** if a new open binding legitimately takes the same address, the existing canonical heartbeat/schedule row follows that canonical address after exact ownership and uniqueness checks. No binding ID is stored in the heartbeat row.
- **Repeated resolution:** all new operations are reads. They create no session, binding, heartbeat row, schedule, or registry entry.
- **Concurrent lifecycle change:** the sink's second DB lookup and existing live acknowledgement check close the race without a transaction spanning async work.

## 10. Compatibility and migration

There is no schema or data migration.

Existing data remains authoritative:

- reused canonical session ID;
- all conversation and memory history attached to it;
- `agent_sessions` membership;
- `session_heartbeat_configs` rows;
- schedules that store the canonical session ID;
- binding and instance rows;
- physical extgate sessions and their heartbeat/schedule rows.

The source already has all required relations and indexes, and `canonical_session_id` already defines the physical-versus-address precedence. A migration would duplicate or split history and violate the approved decision.

## 11. Planned files

Production changes:

- `crates/db/src/queries/gate_binding.rs` — canonical binding lookup and unit tests.
- `crates/db/src/queries/README.md` — document the new generic query contract.
- `crates/actions/src/timed_fire.rs` — default persisted-resolution hook, router method, unique-match behavior, and neutral documentation/tests.
- `crates/extgate/src/fire.rs` — full-session route, DB-aware resolution, ownership check, and sink revalidation.
- `crates/server/src/scheduler.rs` — use persisted resolution with its existing DB connection.
- `crates/server/src/agent_heartbeat.rs` — use persisted resolution for current and explicit sessions.
- `crates/server/src/agent_schedule.rs` — use persisted resolution.
- `crates/server/src/api/schedules.rs` — use persisted resolution for create/update validation.
- Any server test helper that intentionally invokes the production resolution seam — switch it to `resolve_persisted_target`; do not add protocol parsing or protocol fixtures there.

Protocol-owned regression only:

- `crates/nostr/src/gate_provision.rs` test module, plus `crates/nostr/Cargo.toml` dev-dependency only if needed — prove that the existing protocol provisioner reuses its session and that the generic persisted resolver returns the unchanged canonical ID.

Explicitly unchanged:

- all files under `crates/core`;
- schemas and migrations;
- wire protocol, gate client, gateway address generation, and inbound persistence;
- production code in `crates/nostr` and `crates/nostr-gateway`.

## 12. Red tests to add first

### DB query tests

In `gate_binding.rs`:

1. `lookup_canonical_gate_binding_resolves_reused_exact_address` — existing session plus one open binding returns its binding and owner.
2. `lookup_canonical_gate_binding_preserves_physical_session` — physical canonical session still resolves.
3. `lookup_canonical_gate_binding_rejects_closed_or_deleted`.
4. `lookup_canonical_gate_binding_rejects_noncanonical_address_when_physical_exists`.
5. `lookup_canonical_gate_binding_reports_ambiguous_exact_address` — seed two open candidates directly to model corrupt/legacy data.

### Router/extgate tests

1. `resolve_persisted_target_round_trips_binding_address_alias` — generic synthetic address resolves and `build_session_id` returns the same bytes.
2. `resolve_persisted_target_rejects_other_agent`.
3. `resolve_persisted_target_rejects_multiple_descriptors` — actions-level dummy descriptors prove fail-closed ambiguity.
4. Existing static physical descriptor round-trip remains green.
5. Sink test: alias resolved before close but closed before `fire_timed_turn` produces no runtime call/delivery.
6. Sink test: request agent mismatch produces no runtime call/delivery.

### Server tests

Use only generic extgate fixtures and opaque addresses:

1. scheduler rebuild includes an enabled heartbeat row on a reused canonical address.
2. heartbeat get/set/run validation accepts that owned address and rejects another agent.
3. schedule create/update accepts that owned address and rejects another agent.
4. existing physical-session scheduler/tool tests remain green.

### Protocol-owned regression

In `crates/nostr`, provision an existing protocol session through `provision_nostr_gate`, then pass its unchanged session ID to the generic persisted router and assert extgate resolution plus exact build round-trip. No protocol fixture, string, or branch is added to actions, server, extgate, or DB code/tests.

## 13. Validation plan

Focused red/green loop:

```text
cargo test -p opencrab-db gate_binding
cargo test -p opencrab-actions timed_fire
cargo test -p opencrab-extgate --test conformance
cargo test -p opencrab-server scheduler
cargo test -p opencrab-server agent_heartbeat
cargo test -p opencrab-server api::schedules
cargo test -p opencrab-nostr gate_provision
```

Run the repository's shared-layer protocol-leak guard after extgate changes (the existing `no_platform_branch` coverage). Then run formatting and lint checks:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Acceptance observations:

- an enabled reused-address heartbeat produces one scheduler entry;
- `run_one_heartbeat` sends the unchanged canonical session ID to the extgate sink;
- the sink selects the expected live acknowledged binding;
- history/config row counts and session IDs do not change;
- closed, ambiguous, wrong-owner, disconnected, and unacknowledged cases emit no delivery.

## 14. Rollback

Rollback is a code revert of the implementation commit(s). Because the change performs no writes and has no migration, rollback requires no data repair. Existing canonical sessions, history, heartbeat rows, and schedules remain valid; on old code, reused-address timed fire returns to fail-closed behavior. Operators can also disable the affected heartbeat/schedule while rolling back without modifying bindings or history.

## 15. Non-goals

- Migrating or copying reused sessions to physical extgate session IDs.
- Rewriting existing history, memberships, heartbeat rows, or schedules.
- Adding a schema, alias table, cache, background repair, or startup backfill.
- Changing binding creation, provisioning, wire frames, reconnect, or acknowledgement semantics.
- Adding protocol-specific behavior to core, actions, server, extgate, or DB shared code.
- Inferring a binding from a protocol prefix or address shape.
- Falling back to a closed, deleted, disconnected, unacknowledged, wrong-owner, or ambiguous binding.
- Changing heartbeat timing, global gating, prompt text, schedule semantics, delivery mode, or caller identity.
- Implementing production code in this design stage.
