# Issue #1006 S2 TDD evidence — subject and binding authority

Date: 2026-09-24
Stage: S2 only
Starting checkpoint: `9f66c6671591c406616a12bb5085da6c786befcb`
Rollback checkpoint: `9f66c6671591c406616a12bb5085da6c786befcb`
Completion commit: `ef857abbca0c59e3c0faff1efa39ad2b84b6e33c`
Review-fix commit: the commit containing this evidence

## Scope completed

S2 establishes the generic core subject and binding authority required by the approved gateway-process architecture:

- positive, monotonic `INTEGER` subject IDs with allocator state and non-reuse tombstones;
- one active subject association per gateway instance;
- grandfathering only for associations that existed when migration v54 ran;
- opaque, hashed, pair-bound, expiring, single-use subject-association grants;
- exactly one transaction authority for session, membership, and binding creation;
- byte-identical retry idempotence and deterministic conflict behavior;
- atomic concurrent grant consumption and binding creation;
- admin and runtime delegation to the same authority;
- rejection of new bindings for missing or deleted instances.

No S3 ownership, gateway-store migration, runtime fallback, or destructive cleanup work is included.

## RED

Commit `f7b0c6c` introduced the S2 assertion-level RED tests. Before GREEN they exposed:

1. `s2_populated_upgrade_preserves_positive_subject_ids_and_associations_byte_for_byte`
   - schema version remained 53;
   - no subject allocator or v54 safeguards existed.
2. `s2_fresh_schema_installs_allocator_tombstones_and_hashed_grants`
   - fresh schema lacked allocator, tombstone, grant, and binding-session columns.
3. `s2_hard_delete_tombstones_subject_and_allocator_never_reuses_it`
   - deleted subject IDs could be lost and reused.
4. `s2_grant_is_hashed_pair_bound_expiring_and_single_use`
   - no grant issuance or consumption API existed.
5. `s2_concurrent_grant_consumption_has_exactly_one_winner`
   - no atomic single-consumer grant authority existed.
6. `s2_binding_creation_is_byte_idempotent_through_the_generic_authority`
   - binding creation was split among call sites rather than one generic authority.
7. `s2_concurrent_byte_identical_binding_creation_converges_to_one_row`
   - concurrent retries had no shared deterministic authority.
8. `s2_new_first_instance_association_without_grant_is_forbidden`
   - first-instance association could be created without proof.
9. `s2_subject_grant_is_consumed_once_and_exact_retry_needs_no_second_grant`
   - no one-time grant or exact-retry semantics existed.
10. `s2_runtime_and_admin_binding_creation_delegate_to_one_idempotent_authority`
    - runtime and admin paths did not delegate to one service.

A final safety assertion was then added:

11. `s2_deleted_instance_cannot_create_a_binding`
    - RED: a soft-deleted instance accepted a new binding;
    - RED log: `/tmp/issue-1006-s2-deleted-instance-red.log`;
    - GREEN: instance resolution now requires `deleted_at IS NULL` and reports `instance_unknown` through both admin and runtime paths.

Fresh review found two required assertions absent from the matrix at `ef857abbca0c59e3c0faff1efa39ad2b84b6e33c`. The underlying guards already existed, so assertion-level mutation controls retained the RED proof without adding production behavior:

12. `s2_allocator_rejects_decrement_reset_and_delete_and_preserves_high_water`
    - RED control: disabling only the allocator monotonic/delete triggers made the named test fail with `allocator decrement unexpectedly succeeded` (`/tmp/issue-1006-s2-allocator-red.log`, exit 101);
    - GREEN: the unmodified v54 guards reject decrement, reset, and delete, preserve the allocator row, and the next allocation remains above the prior high-water.
13. `s2_grandfathered_association_exact_put_needs_no_grant_and_changes_no_bytes`
    - RED control: forcing only the exact-existing-instance branch to reject made the named test fail with `grandfathered exact PUT required a grant` (`/tmp/issue-1006-s2-grandfathered-red.log`, exit 101);
    - GREEN: a real v53 fixture is migrated through v54; its exact grantless gate-admin PUT succeeds, a genuinely new grantless association fails, and the grandfathered association bytes and complete grant-row set remain unchanged.

Both temporary mutation controls were restored before GREEN validation; `git diff` confirmed no production-source change.

## Minimal GREEN

- Migration v54 installs subject allocation, tombstones, grants, association grandfathering, and binding/session linkage while preserving existing positive IDs and associations.
- `subject.rs` owns issuance and transactional consumption of opaque grants. Only hashes are persisted; agent/subject pair, expiry, and single-use state are checked atomically.
- `CoreBindingService::create_in_tx` is the sole generic session/membership/binding write authority. Admin and runtime call it inside their own immediate transaction and preserve their required audit/protocol behavior.
- Missing and soft-deleted instances return `CreateGateBindingError::Unknown`; call sites expose only `instance_unknown`.
- Existing tests were updated only where S2 intentionally requires an explicit session descriptor or a subject-association grant.
- Large binding tests were moved to `gate_binding_s2_tests.rs` to satisfy the 800-line source gate without production behavior changes.
- The S0 boundary baseline received line-only maintenance for the S2 `subject` module insertion and inherited S1 dev-dependency line drift. Its finding count and classifications remain exactly 451.
- The fresh review fix adds only the two missing named assertions plus a test-harness constructor for a migrated v53 database; no production source, schema, or behavior changed.

## Named GREEN results

```text
cargo test -p opencrab-db s2_ -- --nocapture
12 passed; 0 failed

cargo test -p opencrab-extgate --test conformance s2_ -- --nocapture
4 passed; 0 failed
```

The 11 DB matches include the required rollback assertion:

- `s2_pre_schema_snapshot_restores_byte_identical_fixture`
  - builds a populated pre-S2 v53 fixture;
  - takes a stopped/offline snapshot;
  - upgrades it through v54;
  - restores the snapshot;
  - proves byte-identical database contents, schema version 53, and exact subject/instance association restoration.

## Validation

Passed:

- `cargo test -p opencrab-db --lib --no-fail-fast`
  - 262 passed; 3 ignored; 0 failed.
- `cargo test -p opencrab-extgate --all-targets --no-fail-fast`
  - library 67 passed;
  - conformance 85 passed;
  - production-boundary 1 passed;
  - no-platform-branch 8 passed.
- `cargo test -p opencrab-server --bin opencrab-server --no-fail-fast`
  - 24 passed.
- `cargo test -p opencrab-server --test discord_qc_harness_e2e --no-fail-fast`
  - 40 passed.
- `cargo test -p opencrab-server --test qc_harness_e2e --no-fail-fast`
  - 21 passed.
- `cargo check -p opencrab-nostr`
- `cargo test --workspace --all-features --no-run`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo fmt --all -- --check`
- `bash scripts/check-file-size.sh`
- `bash scripts/check-deps.sh`
- `cargo tree -p opencrab-server --edges no-dev | (! grep -E 'opencrab-(discord|nostr|web)-gateway')`
- `python3 scripts/gateway_boundary_audit.py`
  - 451 classified findings; no unclassified/stale entry; only the two reviewed dev-only QC edges.
- `python3 -m unittest scripts/tests/test_gateway_boundary_audit.py`
  - 19 passed.
- `git diff --check`

The first full-workspace attempt hit an environmental incremental-cache race (`dep-graph.part.bin` disappeared). Retrying with `CARGO_INCREMENTAL=0` ran the workspace and left four inherited, non-S2 targets failing:

- `opencrab-server --lib`: the checked L2 artifact still contains the S1-withdrawn public gate-admin routes and unauthorized response body;
- `opencrab-web-gateway --test core_process_e2e`;
- `opencrab-web-gateway --test web_conversation_create_e2e`;
- `opencrab-web-gateway --test web_mock_contracts_e2e`.

The three Web targets cannot start core because their pre-S2 harness config omits the S1-required protected `gate_admin.listen_socket` and `bootstrap_credential_file`. No S2 source or fixture assumption causes these failures, so they were not changed in this stage. They are recorded in Issue #1011: https://github.com/kojira/opencrab/issues/1011. All S2-relevant DB, extgate, server, Discord QC, Nostr QC, static boundary, format, and clippy checks pass.

## Rollback

- Code rollback point: `9f66c6671591c406616a12bb5085da6c786befcb`.
- Data rollback proof: `s2_pre_schema_snapshot_restores_byte_identical_fixture` passes using a stopped/offline pre-v54 snapshot and byte-for-byte restore comparison.
- Migration preservation proof: `s2_populated_upgrade_preserves_positive_subject_ids_and_associations_byte_for_byte` verifies exact pre/post ID and association tuples.
- No cleanup or irreversible legacy-state deletion occurs in S2.

## Residual risks and deferrals

- The unrelated inherited full-workspace baseline/Web harness failures are isolated in Issue #1011 and do not weaken S2 authority or migration behavior.
- Later gateway-store ownership, projection, freeze, cleanup, and zero-core-change gates remain S3–S11 work and are intentionally untouched.
- `stash@{0}` (`cc26425bc66f685160efeb2139bcaf85ed9dfca1`) remains untouched.
