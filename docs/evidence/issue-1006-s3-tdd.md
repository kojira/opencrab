# Issue #1006 S3 TDD evidence — dynamic operations and platform-neutral routing

Date: 2026-09-24
Stage: S3 only
Starting and rollback checkpoint: `f33424c26f29a4de0e30aef763a75f1f1d0fea6a`
Completion commit: the commit containing this evidence

## Scope completed

S3 versions the dynamic operation declaration and invocation envelopes and makes declaration metadata the sole authority for gateway-operation authorization, dispatch, sub-engine exposure, sharing, effect, and delivery compatibility. Timed fire now carries only canonical generic binding/session identifiers. Shared/server concrete lifecycle registries and operation-name classification fallbacks are removed; generic extgate liveness remains.

No S4 heartbeat migration, gateway-owned identity/policy store, delivery-ledger migration, cleanup, or Issue #1011 work is included.

## Assertion-level RED

The retained RED runs were:

```text
cargo test -p opencrab-extgate s3_declaration_requirement_allows_only_exactly_once -- --nocapture
1 failed: at_most_once_indeterminate was accepted as a declaration requirement
log: /tmp/issue-1006-s3-review-fix-declaration-red.log

cargo test -p opencrab-extgate --test conformance s3_hello_final_delivery_rejects_both_legacy_config_mismatch_directions -- --nocapture
1 failed: operation_driven hello was accepted over automatic legacy config
log: /tmp/issue-1006-s3-review-fix-final-delivery-red.log

cargo test -p opencrab-extgate --test conformance s3_exact_runtime_rejects_explicit_weaker_invocation_before_db_and_wire -- --nocapture
hung after writing the forbidden invoke frame, proving the downgrade was executed
log: /tmp/issue-1006-s3-review-fix-guarantee-red.log

cargo test -p opencrab-gate-client s3_invoke_requires_digest_dispatch_and_effective_guarantee -- --nocapture
compile-time assertion failures: Invoke retained none of the required snapshot fields
log: /tmp/issue-1006-s3-review-fix-gate-client-red.log

cargo test -p opencrab-extgate --test conformance \
  s3_automatic_hello_snapshot_survives_legacy_config_mutation_for_real_continuation \
  -- --nocapture
checkpoint: 2f0ad32 (before the runtime-authority fix)
1 failed after the real timed continuation emitted no say frame once legacy config was changed
log: /tmp/issue-1006-s3-behavioral-automatic-red.log

The exact/global canonical TimedFireRouter fan-out behavior already passed at `2afe535`; the rereview blocker was missing behavioral coverage, not missing production behavior. The new regression was therefore retained honestly as characterization rather than fabricated RED:

cargo test -p opencrab-server --test transport_fire_registry \
  s3_exact_and_global_fixtures_fan_out_through_canonical_timed_fire_routes \
  -- --nocapture
checkpoint: 2afe535
1 passed
log: /tmp/issue-1006-s3-routing-characterization-2afe535.log

python3 -m unittest ...test_s3_generic_caller_role_deferral_requires_exact_finding_identity
1 assertion failure: broad traits.rs classification incorrectly assigned the generic caller role to V10/S3
log: /tmp/issue-1006-s3-review-fix-baseline-red.log
```

```text
cargo test -p opencrab-extgate s3_ -- --nocapture
3 failed, 1 passed
log: /tmp/issue-1006-s3-operations-red.log
```

The failures proved that arbitrary names with complete metadata were rejected, utterance dispatch/effect parity was not enforced, and policy metadata was not fully digest-covered.

```text
cargo test -p opencrab-extgate --test s3_static_boundary -- --nocapture
3 failed, 0 passed
log: /tmp/issue-1006-s3-static-red.log
```

The failures independently detected platform-shaped timed-fire fields, operation-name fallback classification, and the concrete shared/server lifecycle registry.

## Minimal GREEN

- Protocol v3 hello carries operation protocol, final-delivery mode, delivery guarantee, and a complete operation declaration set.
- Every declaration requires validated `authorization`, `dispatch`, `sub_engine`, `sharing`, and `effect`; `dispatch=utterance` iff `effect=utterance`.
- Declaration and runtime capability digests cover all policy and compatibility fields.
- Invocation protocol v1 carries declaration digest, dispatch, effect, and the always-present effective live delivery guarantee.
- Transitional hello validation rejects both `final_delivery`/legacy-config mismatch directions; after acceptance the hello snapshot is the sole runtime authority.
- Declaration requirements accept only optional `exactly_once`; invocation requirements cannot raise beyond or lower the live guarantee, and incompatibility is rejected before DB/wire effects.
- Projections retain the live declaration digest and recheck it at the DB/wire boundary. Gate-client retains its hello snapshot and rejects missing/stale digest, undeclared operations, dispatch/effect drift, and guarantee downgrade before calling the adapter handler.
- `s3_raised_exactly_once_is_rejected_before_db_and_wire` proves an independently raised `exactly_once` requirement is rejected before a `gateway_operation_calls` insert and before an invoke frame.
- `s3_arbitrary_synthetic_operation_projects_and_authorizes_from_metadata` proves an arbitrary `quasar.synthetic-v7` operation is projected without a shared allowlist, denies an undeclared caller before DB/wire effects, and executes for the metadata-authorized owner.
- `s3_projection_rejects_stale_live_declaration_digest_before_db_and_wire` proves reconnect drift cannot authorize from a stale projection.
- Gate-client parser/snapshot assertions cover missing/stale digest, undeclared operation, dispatch/effect mismatch, and guarantee downgrade before the adapter handler.
- `s3_automatic_hello_snapshot_survives_legacy_config_mutation_for_real_continuation` accepts a real automatic hello, mutates legacy config afterward, resolves the canonical generic route, triggers a real timed continuation, and observes exactly one say on that binding.
- `s3_exact_and_global_fixtures_fan_out_through_canonical_timed_fire_routes` resolves distinct exact/global fixtures through `TimedFireRouter`, fans both targets into a collecting production sink, and asserts both canonical binding/session destinations and an exact count of two.
- The prior Python function-name sentinel was removed; it is not behavioral evidence.
- `AgentGatewayLifecycle`, `AgentGatewayRegistry`, and server `AppState.gateways` were removed; liveness is read from extgate.
- Discord and Nostr declarations were migrated to the complete generic metadata contract. CLI and Web continue to use the opaque generic client.
- Large test modules were split without behavior changes to keep every Rust source file below 800 lines.

## S0 burn-down

The reviewed boundary inventory decreased from 451 to 412 findings. Exactly 39 stale findings were removed because their production sites disappeared:

- V08 concrete lifecycle registry/server ownership sites;
- V09 platform-shaped timed-fire and heartbeat routing sites;
- V10 gateway operation-name classification sites;
- directly coupled V11 concrete symbols removed by the same S3 changes.

The two remaining platform-shaped subtask prompt lines were removed. The three exact `GatewayCaller::TrustedUser` sites were semantically classified as generic caller-role naming debt (V11, S5/S10), not dynamic operation routing (V10); an exact-identity mutation test proves moved/duplicated sites remain V10/S3. V08–V10 and V16 have retained closed/static/synthetic proof anchors. No wildcard exception was added. `shared-gateway-name-branch` is zero.

## GREEN validation

Passed:

```text
cargo test -p opencrab-extgate --all-targets --no-fail-fast
72 library; 90 conformance; 1 production-boundary; 8 no-platform; 3 S3 static

cargo test -p opencrab-gate-client --all-targets --no-fail-fast
30 library; 7 turn-origin

cargo test -p opencrab-discord-gateway --all-targets --no-fail-fast
61 library

cargo test -p opencrab-nostr-gateway --all-targets --no-fail-fast
72 library

cargo test -p opencrab-server --test transport_fire_registry --test utterance_parity --no-fail-fast
3 transport-fire; 1 utterance-parity

cargo test -p opencrab-server --lib --no-fail-fast
493 passed

cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
bash scripts/check-file-size.sh
bash scripts/check-deps.sh
cargo tree -p opencrab-server --edges no-dev | (! grep -E 'opencrab-(discord|nostr|web)-gateway')
python3 scripts/gateway_boundary_audit.py
python3 -m unittest scripts/tests/test_gateway_boundary_audit.py
55 passed

cargo test -p opencrab-db canonical_lookup_resolves_distinct_generic_aliases_without_writes
1 passed

git diff --check
```

The gate-client focused run initially exposed two stale protocol-v2 unit fixtures. They were updated to assert the required protocol-v3 hello capabilities and invocation metadata, then the complete gate-client target set passed.

Per the S3 continuation contract, the unrelated full-workspace Issue #1011 failures were not rerun or changed.

## Rollback and residual scope

- Code rollback point: `f33424c26f29a4de0e30aef763a75f1f1d0fea6a`.
- S3 contains no schema migration, destructive cleanup, or gateway-store state mutation.
- V08 remains partially mapped to S5 for daemon-owned concrete child lifecycle. Later-stage heartbeat and split-ledger work remains governed by S4/S7, but no surviving baseline item is incorrectly owned by completed S3 V09/V10 routing work.
- `stash@{0}` remains untouched at `cc26425bc66f685160efeb2139bcaf85ed9dfca1`.
