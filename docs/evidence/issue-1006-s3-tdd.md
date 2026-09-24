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
- Invocation protocol v1 carries declaration digest, effect, and the effective delivery requirement.
- Runtime compatibility rejects operation-driven delivery without an utterance operation and rejects a guarantee weaker than either the declaration or an independently raised invocation requirement.
- `s3_raised_exactly_once_is_rejected_before_db_and_wire` proves an independently raised `exactly_once` requirement is rejected before a `gateway_operation_calls` insert and before an invoke frame.
- `s3_arbitrary_synthetic_operation_projects_and_authorizes_from_metadata` proves an arbitrary `quasar.synthetic-v7` operation is projected without a shared allowlist, denies an undeclared caller before DB/wire effects, and executes for the metadata-authorized owner.
- Timed fire and scheduler routing use only generic binding/session targets and one generic live sink.
- `AgentGatewayLifecycle`, `AgentGatewayRegistry`, and server `AppState.gateways` were removed; liveness is read from extgate.
- Discord and Nostr declarations were migrated to the complete generic metadata contract. CLI and Web continue to use the opaque generic client.
- Large test modules were split without behavior changes to keep every Rust source file below 800 lines.

## S0 burn-down

The reviewed boundary inventory decreased from 451 to 414 findings. Exactly 37 stale findings were removed because their production sites disappeared:

- V08 concrete lifecycle registry/server ownership sites;
- V09 platform-shaped timed-fire and heartbeat routing sites;
- V10 gateway operation-name classification sites;
- directly coupled V11 concrete symbols removed by the same S3 changes.

All surviving entries retain generated `_metadata_for` classifications and received line-only refreshes where S3 moved code. V08 and V16 now use retained S3 static/synthetic proof anchors. No wildcard exception was added. `shared-gateway-name-branch` is now zero.

## GREEN validation

Passed:

```text
cargo test -p opencrab-extgate --all-targets --no-fail-fast
71 library; 87 conformance; 1 production-boundary; 8 no-platform; 3 S3 static

cargo test -p opencrab-gate-client --all-targets --no-fail-fast
28 library; 7 turn-origin

cargo test -p opencrab-discord-gateway --all-targets --no-fail-fast
61 library

cargo test -p opencrab-nostr-gateway --all-targets --no-fail-fast
72 library

cargo test -p opencrab-server --test transport_fire_registry --test utterance_parity --no-fail-fast
2 transport-fire; 1 utterance-parity

cargo test -p opencrab-server --lib --no-fail-fast
493 passed

cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
bash scripts/check-file-size.sh
bash scripts/check-deps.sh
cargo tree -p opencrab-server --edges no-dev | (! grep -E 'opencrab-(discord|nostr|web)-gateway')
python3 scripts/gateway_boundary_audit.py
python3 -m unittest scripts/tests/test_gateway_boundary_audit.py
git diff --check
```

The gate-client focused run initially exposed two stale protocol-v2 unit fixtures. They were updated to assert the required protocol-v3 hello capabilities and invocation metadata, then the complete gate-client target set passed.

Per the S3 continuation contract, the unrelated full-workspace Issue #1011 failures were not rerun or changed.

## Rollback and residual scope

- Code rollback point: `f33424c26f29a4de0e30aef763a75f1f1d0fea6a`.
- S3 contains no schema migration, destructive cleanup, or gateway-store state mutation.
- V08 remains partially mapped to S5 for daemon-owned concrete child lifecycle; V09 continues in S4/S7 for generic heartbeat and split-ledger proof; V10 continues in S7 for delivery guarantee proof. S3 closes only the shared/server seams assigned to this stage.
- `stash@{0}` remains untouched at `cc26425bc66f685160efeb2139bcaf85ed9dfca1`.
