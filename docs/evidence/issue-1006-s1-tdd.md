# Issue #1006 S1 TDD evidence

Date: 2026-09-23
Base: `f21c187a65226e8f7c0bce0c0ab8a41c86474b7a`

## Rollback checkpoint

The pre-S1 rollback point is the approved base above. S1 was split into semantic commits:

- `4783e66` — v53 platform-neutral principal/scope/audit migration and transition tests
- `f3b6e61` — strict manifest, bootstrap/restart, authorizer, operator primitives, audit, and socket preparation
- `35c1dc0` — required `[gate_admin]`, protected UDS startup ordering, and public-route isolation
- `63ccb87`, `12576a5`, `fd25310` — file-size split, plaintext redaction/zeroization, read-only restart, and inode-safe cleanup hardening

Rollback must stop all listeners, restore the pre-S1 database and binary/config tuple together, and remove only the captured S1 socket inode. No bearer is recorded here.

## RED

The earlier S1 implementation retained only the two preimplementation REDs below (schema and
public isolation). It did **not** retain a complete assertion-level RED matrix; this record does
not reconstruct or fabricate one after the fact.

### Follow-up review-blocker REDs against `be2a57c`

The follow-up first added named assertions with no production changes. Exact outputs are retained
at `/tmp/issue-1006-s1-followup-red-{auth,atomicity,paths,startup}.txt`.

- `handlers_authenticate_before_parsing_or_lookup_and_audit_denials_and_authorized_errors`:
  expected unauthorized before malformed-body parsing, but received HTTP `400` instead of `401`.
- `handler_mutation_and_required_audit_commit_atomically_and_conflicts_are_audited`:
  after an injected success-audit failure, `gate_instances` contained `1` row instead of `0`.
- `manifest_and_socket_paths_use_directory_fd_no_follow_operations`: failed with
  `manifest opening must use a directory-fd component walk`.
- `gate_admin_startup_fault_injection_covers_every_pre_listener_stage`: failed with
  `missing pre-listener fault injection for migration`.

These are genuine failures from clean HEAD `be2a57cb8128f88e7230e5b68c2464fba6dc32e6`.
The real HTTP/1.1-over-UDS assertion is added in this follow-up alongside the startup-order
implementation; unlike the four gaps above, the underlying Axum/UDS transport was already capable
of serving HTTP, so no contrary preimplementation failure is claimed.

Named RED assertions were added before the original production changes.

### Security schema

Command:

```text
cargo test -p opencrab-db gate_admin_security_schema_is_installed_on_fresh_and_populated_databases
```

Exact pre-change failure:

```text
test schema::migration_tests::gate_admin_security_schema_is_installed_on_fresh_and_populated_databases ... FAILED
missing gate_admin_principals
```

Raw output was retained during implementation at `/tmp/issue-1006-s1-red-schema.txt`.

### Public isolation

Command:

```text
cargo test -p opencrab-server --test webgate_static_audit public_gate_admin_routes_are_absent_after_s1 -- --exact
```

Exact pre-change failure:

```text
S1 requires all six gate-admin operations to be absent from public TCP:
[("/api/gate-bindings/{binding_id}", ["DELETE", "PUT"]),
 ("/api/gate-instances/{instance_id}", ["DELETE", "GET", "PUT"]),
 ("/api/gate-instances/{instance_id}/revisions", ["POST"])]
```

Raw output was retained during implementation at `/tmp/issue-1006-s1-red-public.txt`.

## GREEN coverage

Implemented fixtures cover:

- fresh/populated v53 migration, transactional rollback, legal seal/revoke and illegal reseal/unseal/unrevoke/combined/scope-after-seal transitions;
- exact-idempotent read-only restart, metadata/token/scope conflict, unsealed-state refusal, duplicate bearer refusal, and bounded rotation/revocation;
- strict versioned JSON, unknown/duplicate fields, exact 32-byte unpadded base64url, EUID/mode/type/symlink checks, zeroizing buffers, and redacted `Debug`;
- full credential scans requiring exactly one match, malformed/wrong/expired/revoked/operation/subject/instance/namespace denial, and synthetic duplicate-match no-union behavior;
- append-only sanitized audit and a generic outer-transaction/savepoint primitive proving mutation/audit atomicity and audit-failure rollback;
- private UDS mode/owner, stale path refusal, symlink/insecure parent refusal, and replacement-inode-safe cleanup;
- database-backed success for all six protected operations and public-router 404 for all six methods;
- no S2 allocator/grant/tombstone tables or seventh route.

## Validation completed

```text
cargo test -p opencrab-db --lib
251 passed; 0 failed; 3 ignored

cargo test -p opencrab-extgate --all-targets
62 unit + 81 conformance + 8 static tests passed (before final focused hardening)

cargo test -p opencrab-extgate gate_admin_security --no-fail-fast
11 passed

cargo test -p opencrab-extgate socket_ --no-fail-fast
2 S1 socket tests and existing listener test passed

cargo test -p opencrab-server --lib all_six_gate_admin_operations_are_404_on_public_tcp_router
passed

cargo test -p opencrab-server --test webgate_static_audit
9 passed

cargo check -p opencrab-server --all-targets
passed

bash scripts/check-file-size.sh
OK: every Rust source file is at most 800 lines

cargo fmt --all
git diff --check
passed
```

## Validation not completed in this checkpoint

The parent directed the worker to stop broad validation near the tool limit. Therefore the required 53 S0 mutation tests, exact 454-finding audit, dependency check, Cargo metadata/no-dev tree, full workspace tests, and full workspace all-target/all-feature Clippy were **not rerun after S1**. The earlier S0 evidence remains at `docs/evidence/issue-1006-s0-tdd.md`, but it is not a substitute for an S1 rerun.

## Independent-review blockers / residual risks

This checkpoint is not ready to merge without follow-up implementation review:

1. The generic `audited_mutation` primitive has the required outer transaction/savepoint behavior, but the existing six HTTP handlers still perform their domain mutation and success audit as separate commits. A handler audit failure can therefore occur after mutation commit, and authorized conflicts are not consistently audited.
2. Requests that fail target lookup or body parsing before database authorization do not all emit a sanitized audit row and can retain pre-S1 not-found/bad-request distinctions instead of the single credential-failure response.
3. Symlink-component validation uses metadata checks followed by ordinary open/bind, rather than a directory-fd/openat-style component walk, leaving a path replacement race.
4. Startup-order failure injection and a real HTTP/1.1-over-UDS process test were not completed; current coverage proves preparation helpers and router behavior in-process.
5. The complete S1 RED matrix was not captured assertion-by-assertion before implementation; only the schema and public-isolation RED outputs above are retained.

These are S1-owned issues and must not be deferred into S2.
