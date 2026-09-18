# Design: extgate binding-address aliases for timed fire

Issue: [#996](https://github.com/kojira/opencrab/issues/996)

Status: implementation-ready

## 1. Problem and decision

A generic extgate binding can reuse a session when `gate_bindings.address` is byte-equal to an existing `sessions.id`. `canonical_session_id` then returns that address, and inbound records history in the reused session. Timed-fire routing does not follow the same rule: `ExtgateFire::parse` accepts only a physical `extgate-<binding UUID>` ID. Heartbeat tools, schedule validation, and scheduler rebuild therefore reject an enabled reused-address session before the extgate sink can run.

The approved decision is to keep the existing canonical session and add a generic, exact binding-address alias lookup. We will not move history or configuration to a physical extgate session. Because this lookup starts with `gate_bindings.address`, the next DB migration also adds a generic address-first partial index over open bindings. That migration changes indexing only; it rewrites no session, history, membership, heartbeat, schedule, instance, or binding row.

This is an extgate invariant, not a protocol rule. No shared implementation may recognize a protocol kind, protocol session prefix, protocol-specific type, or protocol topology. Protocol-specific regression setup belongs only to `crates/nostr` or `crates/nostr-gateway`. There is no change under `crates/core`.

## 2. Ownership

| Concern | Owner |
|---|---|
| Canonical session selection and exact session-to-binding lookup | `crates/db/src/queries/gate_binding.rs` |
| Address-first open-binding index and migration/rollback contract | next numbered migration (`v51` at this source revision) under `crates/db/src/schema` |
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

1. The next numbered DB migration adds the address-first partial index described in §5. It changes no rows.
2. Binding creation and inbound remain unchanged.
3. Every server path that validates a stored/current session uses a new DB-aware router entry point, `resolve_persisted_target(conn, session_id, agent_id)`.
4. During every scheduler rebuild, **each enabled heartbeat row and each enabled schedule row** passes that same resolver before an `Entry` is created. A failed resolution skips the row fail-closed; neither loop has a syntax-only bypass.
5. The router asks each descriptor to resolve the persisted session. The trait's default implementation delegates to the existing static `parse`, preserving other transports.
6. `ExtgateFire` overrides persisted resolution. It asks the DB query layer for the one open, non-deleted binding whose canonical session is exactly `session_id`.
7. Extgate accepts the result only when its owning `agent_id` exactly equals the requested agent. It returns a target whose `route` is the full canonical session ID.
8. `ExtgateFire::build_session_id` returns that full route. Thus `run_one_heartbeat` places the original canonical session ID, not a synthesized physical ID, into `TimedFireRequest`.
9. `ExtgateTimedFireSink` performs the same canonical-session lookup again, verifies the requested agent again, resolves binding context, and then applies the existing live-instance and acknowledged-binding checks.
10. The existing said-less extgate turn runs against the reused canonical session, preserving history, delivery behavior, locks, continuation, and completion handling.

The second lookup is intentional TOCTOU protection: closing/deleting/replacing a binding between scheduler resolution and sink receipt must stop delivery. A schedule `Entry` does not need to retain the resolved `FireTarget` because its existing execution path consumes the canonical session directly; the successful target is an admission proof and may be discarded only after `resolve_persisted_target` returns it. Heartbeat keeps the target as today. Both row types still use the identical resolver and fail-closed outcomes before entry creation.

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

Use separate queries rather than an `OR`: the physical candidate uses the binding key; the alias candidate is exactly:

```sql
SELECT b.binding_id, b.instance_id, b.address, a.agent_id
FROM gate_bindings AS b
JOIN gate_instances AS i ON i.instance_id = b.instance_id
JOIN agents AS a ON a.subject_id = i.subject_id
WHERE b.address = ?1
  AND b.closed_at IS NULL
  AND i.deleted_at IS NULL
ORDER BY b.binding_id;
```

The address predicate is byte-exact and remains the first restriction. The query does not use `LIKE`, a prefix, normalization, a protocol kind, or an address parser. `canonical_session_id` filtering and ambiguity counting then run as specified above.

### 5.1 Address-first index migration

At this source revision the latest schema is v50, so implementation adds the next migration as v51 with this exact DDL:

```sql
CREATE INDEX idx_gate_bindings_open_address_lookup
ON gate_bindings(address, binding_id, instance_id)
WHERE closed_at IS NULL;
```

`address` is deliberately first so lookup is independent of instance. `binding_id` and `instance_id` make the candidate read covering where SQLite permits; the existing unique partial index on `(instance_id, address)` remains unchanged because it enforces a different invariant. The new index is non-unique: ambiguity remains data that the resolver must detect and reject, not silently prevent or choose around.

Add v51 to the ordered migration catalog. The migration runs in the existing per-version transaction. Fresh databases and upgrades from v50 both receive the same index through the numbered migration path; historical migration bodies are not edited.

Migration failure is fail-loud: an index creation error (including an object already occupying the approved index name) rolls back the v51 transaction, leaves `PRAGMA user_version = 50`, preserves all rows, and prevents application startup. The DDL intentionally omits `IF NOT EXISTS`; the migration catalog makes normal initialization idempotent, while a conflicting or manually altered schema must not be stamped as valid. Because failed transactional DDL leaves no index, a normal retry safely creates it. The implementation must not catch that failure and continue with an unindexed production scheduler. Lookup query/storage errors remain `Err`; extgate logs them and scheduler/tool validation returns no target.

### 5.2 Query-plan and scale proof

Migration/schema tests inspect `PRAGMA index_list('gate_bindings')` and `PRAGMA index_info('idx_gate_bindings_open_address_lookup')` to pin: non-unique, partial, and ordered columns `(address, binding_id, instance_id)`. An upgrade fixture starts at v50 with representative open and closed rows, initializes twice, and proves row counts/values are byte-identical while the index exists once.

A deterministic query-plan test seeds at least 10,000 nonmatching open/closed bindings plus one exact address and runs `EXPLAIN QUERY PLAN` for the address candidate SQL. It must name `idx_gate_bindings_open_address_lookup` and must not report a full scan of `gate_bindings`. An ignored scale test seeds at least 100,000 bindings, executes repeated hit and miss lookups, asserts correct results, prints timing with `--nocapture`, and imposes no flaky wall-clock threshold; index selection is the pass/fail performance invariant.

`enabled` is deliberately not part of identity resolution. A stopped/disabled instance must remain inspectable by heartbeat tools, matching physical-session behavior. Actual heartbeat delivery still requires a live instance and binding acknowledgement in `ExtgateTimedFireSink`.

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

Add `TimedFireRouter::resolve_persisted_target(conn, session_id, agent_id)`. It calls `resolve_persisted` on all registered descriptors and returns a target only when there is exactly one distinct match. Zero or multiple matches return `None`; multiple matches emit a warning without exposing session content. This makes dynamic alias collisions fail closed rather than depend on registration order. This one method is the mandatory admission gate for heartbeat tools, schedule APIs, enabled heartbeat rows, and enabled schedule rows.

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
| DB lock/query failure | warning; tool calls fail closed with a retryable error, and schedule HTTP create/update returns 500 |
| Binding closes after target resolution | sink revalidation rejects |
| Instance is disconnected or binding is not acknowledged | existing sink warning; no turn |

The physical static parser remains available only for format/collision checks. Product validation and scheduler enumeration use the persisted resolver, so static syntax alone does not authorize a fire.

## 9. Binding lifecycle, reconnect, and idempotency

- **Creation/reuse:** no change. Existing transaction logic chooses physical creation or address reuse.
- **Reconnect:** hello replay and bind acknowledgement remain unchanged. Persistent alias lookup continues to return the same binding while disconnected, so valid heartbeat/schedule rows still pass identity admission; the heartbeat sink refuses delivery until acknowledgement returns. After reconnect and acknowledgement, the same rows and binding resolve and fire without repair or rewrite.
- **Close:** a closed binding stops resolving immediately. Both heartbeat and schedule rows are omitted at scheduler entry creation. Existing rows remain untouched and can become routable again only through a valid new open binding for the same canonical address and owner.
- **Delete:** a deleted instance never resolves; neither persisted row type creates a scheduler entry.
- **Replacement:** if a new open binding legitimately takes the same address, the existing canonical heartbeat/schedule row follows that canonical address after exact ownership and uniqueness checks. No binding ID is stored in either timed-fire row.
- **Repeated resolution:** after the one-time index migration, all resolution operations are reads. They create or update no session, history, membership, instance, binding, heartbeat row, schedule, or registry entry. Tests snapshot all affected table counts and values around repeated hit/miss resolution.
- **Concurrent lifecycle change:** the sink's second DB lookup and existing live acknowledgement check close the race without a transaction spanning async work.

## 10. Compatibility and migration

There is one schema migration: the additive v51 partial index in §5.1. It performs no data migration and no row rewrite. Existing data remains authoritative:

- reused canonical session ID;
- all conversation and memory history attached to it;
- `agent_sessions` membership;
- `session_heartbeat_configs` rows;
- schedules that store the canonical session ID;
- binding and instance rows;
- physical extgate sessions and their heartbeat/schedule rows.

The source already has all required relations, and `canonical_session_id` already defines physical-versus-address precedence. The only missing scale primitive is an address-first open-binding index; v51 adds exactly that. An alias table, backfill, or row migration would duplicate or split authority and remains prohibited.

## 11. Planned files

Production changes:

- `crates/db/src/queries/gate_binding.rs` — canonical binding lookup, exact address SQL, query-plan/scale tests, and no-write idempotency tests.
- `crates/db/src/queries/README.md` — document the new generic query and index contract.
- `crates/db/src/schema/migrations/v51.rs` — additive address-first partial index only.
- `crates/db/src/schema/migrations/mod.rs` — append v51 to the ordered catalog.
- `crates/db/src/schema/migration_tests.rs` and a focused v51 test file under `crates/db/src/schema/tests` — fresh/upgrade/idempotency/failure/row-preservation/index-shape coverage.
- `crates/actions/src/timed_fire.rs` — default persisted-resolution hook, router method, unique-match behavior, and neutral documentation/tests.
- `crates/extgate/src/fire.rs` — full-session route, DB-aware resolution, ownership check, and sink revalidation.
- `crates/server/src/scheduler.rs` — gate **both** enabled heartbeat and enabled schedule rows through persisted resolution before either `Entry` is pushed, using the existing DB connection.
- `crates/server/src/agent_heartbeat.rs` — use persisted resolution for current and explicit sessions.
- `crates/server/src/agent_schedule.rs` — use persisted resolution.
- `crates/server/src/api/schedules.rs` — use persisted resolution for create/update validation.
- Any server test helper that intentionally invokes the production resolution seam — switch it to `resolve_persisted_target`; do not add protocol parsing or protocol fixtures there.

Protocol-owned regression only:

- `crates/nostr/src/gate_provision.rs` test module, plus `crates/nostr/Cargo.toml` dev-dependency only if needed — prove that the existing protocol provisioner reuses its session and that the generic persisted resolver returns the unchanged canonical ID.

Explicitly unchanged:

- all files under `crates/core`;
- every schema object except the one generic v51 index;
- wire protocol, gate client, gateway address generation, and inbound persistence;
- production code in `crates/nostr` and `crates/nostr-gateway`.

## 12. Red tests to add first

### Migration, index, and DB query tests

1. `v50_to_v51_adds_open_address_lookup_index_without_rewriting_rows` — seed open/closed bindings and related rows, migrate twice, assert latest version, one index, and byte-identical row values/counts.
2. `fresh_schema_has_open_address_lookup_index` — inspect `index_list`/`index_info` for non-unique, partial, address-first `(address, binding_id, instance_id)` shape.
3. `v51_index_failure_rolls_back_and_keeps_version_50` — occupy the approved index name with a conflicting schema object, prove fail-loud transaction rollback, unchanged rows, and version 50; remove the conflict and prove retry succeeds.
4. `v51_index_rollback_to_v50_and_forward_reapply_preserves_rows` — apply v51, follow the documented stopped-process rollback (`DROP INDEX`, version 50), verify old-shape readability, then initialize forward again and prove identical rows plus restored index.
5. `lookup_canonical_gate_binding_uses_open_address_lookup_index` — seed at least 10,000 mixed rows; `EXPLAIN QUERY PLAN` must name the new index and not scan `gate_bindings`.
6. ignored `lookup_canonical_gate_binding_scale` — at least 100,000 rows, repeated exact hits/misses, correct results, printed timing, no wall-clock assertion.
7. `lookup_canonical_gate_binding_resolves_reused_exact_address` — existing session plus one open binding returns its binding and owner.
8. `lookup_canonical_gate_binding_preserves_physical_session` — physical canonical session still resolves.
9. `lookup_canonical_gate_binding_rejects_closed_or_deleted`.
10. `lookup_canonical_gate_binding_rejects_noncanonical_address_when_physical_exists`.
11. `lookup_canonical_gate_binding_reports_ambiguous_exact_address` — seed two open candidates directly to model corrupt/legacy data.
12. `repeated_canonical_binding_resolution_writes_nothing` — snapshot `PRAGMA user_version`, all relevant row counts/values, and `Connection::total_changes`; repeat matching, missing, wrong-owner-at-router, and ambiguous resolutions; assert snapshots and total changes are unchanged.

### Router/extgate tests

1. `resolve_persisted_target_round_trips_binding_address_alias` — generic synthetic address resolves and `build_session_id` returns the same bytes.
2. `resolve_persisted_target_rejects_other_agent`.
3. `resolve_persisted_target_rejects_multiple_descriptors` — actions-level dummy descriptors prove fail-closed ambiguity.
4. Existing static physical descriptor round-trip remains green.
5. Sink test: alias resolved before close but closed before `fire_timed_turn` produces no runtime call/delivery.
6. Sink test: request agent mismatch produces no runtime call/delivery.
7. `timed_fire_alias_disconnected_is_not_delivered` — persistent resolution succeeds, but no live instance means no runtime call or delivery.
8. `timed_fire_alias_unacknowledged_is_not_delivered` — live instance without this binding in `acknowledged` means no runtime call or delivery.
9. `timed_fire_alias_reconnect_after_ack_delivers_once` — the same unchanged session/config first fails while disconnected, then reconnect/ack permits exactly one turn; no binding/config/session rewrite occurs.

### Server tests

Use only generic extgate fixtures and opaque addresses. Exercise `rebuild_entries`, not a test-only approximation:

1. enabled heartbeat on a valid reused address creates exactly one heartbeat entry.
2. enabled schedule on the same valid alias creates exactly one schedule entry.
3. closed binding creates neither heartbeat nor schedule entry.
4. deleted instance creates neither heartbeat nor schedule entry.
5. ambiguous canonical binding creates neither heartbeat nor schedule entry.
6. wrong-owner row creates neither heartbeat nor schedule entry.
7. malformed/no-match rows remain fail-closed for both row types.
8. heartbeat get/set/run and schedule create/update accept the owned alias and reject another agent through the same router method.
9. existing physical-session scheduler/tool tests remain green.
10. repeated scheduler rebuilds over unchanged heartbeat/schedule rows produce stable entry sets and write no DB state.
11. poisoned/unavailable DB acquisition during persisted resolution returns an explicit fail-closed tool error or HTTP 500; it never panics or degrades into a bad-target 400.

### Protocol-owned regression

In `crates/nostr`, provision an existing protocol session through `provision_nostr_gate`, then pass its unchanged session ID to the generic persisted router and assert extgate resolution plus exact build round-trip. No protocol fixture, string, or branch is added to actions, server, extgate, or DB code/tests.

## 13. Validation plan

Focused red/green loop:

```text
cargo test -p opencrab-db v51
cargo test -p opencrab-db gate_binding
cargo test -p opencrab-db lookup_canonical_gate_binding_scale -- --ignored --nocapture
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

- the v51 migration adds only `idx_gate_bindings_open_address_lookup`, preserves all rows, and is retry-safe;
- the exact-address candidate query selects that index at 10,000+ rows and the ignored 100,000-row scale probe remains correct;
- an enabled reused-address heartbeat and an enabled reused-address schedule each produce one scheduler entry only after `resolve_persisted_target` succeeds;
- closed, deleted, ambiguous, wrong-owner, malformed, and missing bindings produce no heartbeat or schedule entry;
- `run_one_heartbeat` sends the unchanged canonical session ID to the extgate sink;
- disconnected and unacknowledged bindings produce no delivery, while reconnect plus acknowledgement permits one delivery without data repair;
- repeated resolver calls and scheduler rebuilds do not change history/config/gate rows, session IDs, schema version, or `total_changes`;
- the sink selects only the expected live acknowledged binding.

## 14. Rollback

The preferred rollback build reverts resolver/scheduler behavior but retains the v51 migration catalog entry and harmless additive index. That build can open a v51 database, ignores the unused index, and requires no data repair; reused-address timed fire returns to fail-closed behavior while all canonical sessions, history, heartbeat rows, and schedules remain intact.

If an exact pre-v51 binary must be restored, its downgrade guard will correctly reject `user_version = 51`. With the application stopped, use one explicit transaction to `DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup` and set `PRAGMA user_version = 50`, then start the old binary. No table or row rollback is required. Re-deploying the forward build recreates the index through v51. A failed v51 application leaves version 50 automatically as described in §5.1.

Operators may disable affected heartbeat/schedule rows during rollback without modifying bindings or history.

## 15. Non-goals

- Migrating or copying reused sessions to physical extgate session IDs.
- Rewriting existing history, memberships, heartbeat rows, or schedules.
- Adding any schema object beyond the approved generic address-first partial index; no alias table, cache, background repair, or startup backfill.
- Changing binding creation, provisioning, wire frames, reconnect, or acknowledgement semantics.
- Adding protocol-specific behavior to core, actions, server, extgate, or DB shared code.
- Inferring a binding from a protocol prefix or address shape.
- Falling back to a closed, deleted, disconnected, unacknowledged, wrong-owner, or ambiguous binding.
- Changing heartbeat timing, global gating, prompt text, schedule semantics, delivery mode, or caller identity.
- Implementing production code in this design stage.
