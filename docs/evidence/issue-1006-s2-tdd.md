# Issue #1006 S2 TDD evidence — paused checkpoint

Base: `6b25553b12fa3c3e3c1ad9efc05dbd529b6d8450` (approved S1).
Scope: S2 structural safeguards only. Work was paused by user direction before final validation/review.

## Assertion-level RED

Commit `f7b0c6c` added the initial named S2 assertions before production changes.

```text
cargo test -p opencrab-db s2_ -- --nocapture
```

Exact result: **4 tests, 4 failures**:

- `s2_populated_upgrade_preserves_positive_subject_ids_and_associations_byte_for_byte`: `no such table: subject_id_allocator`;
- `s2_fresh_schema_installs_allocator_tombstones_and_hashed_grants`: `missing S2 table subject_id_allocator` (`left: 0`, `right: 1`);
- `s2_hard_delete_tombstones_subject_and_allocator_never_reuses_it`: `no such table: subject_tombstones`;
- `s2_binding_creation_is_byte_idempotent_through_the_generic_authority`: `UNIQUE constraint failed: sessions.id` on the byte-identical retry.

Raw output was retained at `/tmp/issue-1006-s2-red-db.log` for this worktree session.

```text
cargo test -p opencrab-extgate --test conformance \
  s2_new_first_instance_association_without_grant_is_forbidden -- --exact --nocapture
```

Exact result: **1 test, 1 failure**. The unauthorized first association returned `201 Created` rather than the asserted `409 Conflict`, and persisted the instance. Raw output: `/tmp/issue-1006-s2-red-extgate.log`.

A later named cross-entry-point assertion was replayed against detached pre-production commit `f7b0c6c`:

```text
cargo test -p opencrab-extgate --test conformance \
  s2_runtime_and_admin_binding_creation_delegate_to_one_idempotent_authority \
  -- --exact --nocapture
```

Exact RED: **1 test, 1 failure**, `expected frame not received` on the byte-identical runtime retry because the split runtime path attempted duplicate session creation. Raw output: `/tmp/issue-1006-s2-red-runtime-authority.log`. The temporary detached worktree was removed afterward.

## GREEN reached before pause

Commit `a6e4a69` added v54 subject allocator/tombstone/grant schema, immutable grandfathering state, hard-delete tombstoning, and `CoreBindingService` idempotence. Subsequent uncommitted-at-the-time work added hashed random grant issue/consume, grant-required first instance association, explicit session envelopes, and admin/runtime delegation to `CoreBindingService`; it is preserved in the pause commit recorded by the implementation artifact.

Passing checks reached:

```text
cargo test -p opencrab-db s2_ -- --nocapture
8 passed; 0 failed

cargo test -p opencrab-extgate --test conformance --no-fail-fast
83 passed; 0 failed

cargo test -p opencrab-extgate --test conformance \
  s2_runtime_and_admin_binding_creation_delegate_to_one_idempotent_authority \
  -- --exact --nocapture
1 passed; 0 failed
```

The 83-test conformance run preceded addition of the final cross-entry-point test; that final test passed focused.

## Pause state and incomplete validation

A full `cargo test -p opencrab-db --lib --no-fail-fast` initially exposed migration-test rerun compatibility defects. After one fix it reached **251 passed, 8 failed, 3 ignored**. The remaining failures were isolated to old partial migration fixtures and v44 rollback helpers; minimal fixture prerequisite/cleanup edits were made, but the required rerun was interrupted by the user pause and remains outstanding.

Not yet completed after the latest edits:

- full DB library rerun;
- full extgate all-target rerun after the final test;
- S0 54-test boundary suite and exact 451-finding audit;
- dependency/file-size checks, metadata/no-dev tree, fmt, Clippy, and full affected workspace tests;
- independent implementation review and rollback snapshot rehearsal.

No S3+, concrete gateway fields, live migration/data, deployment, README, or Issue #1007 robustness work was intentionally added. `stash@{0}` was not applied, modified, or dropped.
