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

## Follow-up review-blocker GREEN (from `be2a57c`)

The follow-up RED assertions above are now GREEN. The six handlers authenticate the credential and
operation before body parsing or target lookup, then revalidate target scope. Denials collapse to
the byte-stable unauthorized response and append a target-free sanitized audit. Authenticated
bad-request/not-found outcomes append principal-only audit rows; target-authorized success,
conflict, and store outcomes use one outer transaction plus savepoint so an audit failure rolls
back domain mutation. All six operations use this contract.

Manifest resolution now uses an `openat(O_NOFOLLOW)` directory-fd component walk. Admin socket
creation is relative to the held parent fd in a syscall-only fork child and transfers the listening
fd with `SCM_RIGHTS`; cleanup atomically quarantines with `renameat`, verifies identity, and only
then uses `unlinkat`, so a replacement inode is never unlinked. Component/final replacement tests
are GREEN. Unsupported non-Unix behavior remains fail-closed through the crate's existing Unix-only
platform boundary.

Startup has fault injection at migration, credential bootstrap, socket preparation, and router
preparation, all in the synchronous pre-listener phase. A real raw HTTP/1.1-over-UDS test proves
protected authenticated success and unrelated-route 404 behavior.

### Reviewed boundary baseline transition

The old 454-entry baseline correctly failed stale after S1 public isolation. With supervisor
approval, exactly the three S1-owned `public-gate-admin-reachable` entries were removed and exactly
seven otherwise unchanged reviewed identities received mechanical line updates. A named transition
assertion first failed `454 != 451`, then passed with 451 current findings and zero public gate-admin
reachability findings. No finding classification, owner, expiry, or detector policy changed.

### Final validation

- focused follow-up handler/path/startup/real-UDS tests: passed;
- `cargo test -p opencrab-db --lib`: 251 passed, 3 ignored;
- `cargo test -p opencrab-extgate --all-targets`: 70 unit, 81 conformance, 8 static passed;
- `cargo test -p opencrab-server --lib`: 497 passed;
- `cargo test -p opencrab-server --test webgate_static_audit`: 9 passed;
- startup fault-injection binary test: passed;
- Python boundary mutation suite: 54 passed (the retained 53 S0 assertions plus the S1 transition assertion);
- `python3 scripts/gateway_boundary_audit.py`: exactly 451 classified findings after the authorized three-entry S1 burn-down;
- `bash scripts/check-deps.sh` and `bash scripts/check-file-size.sh`: passed;
- Cargo metadata: Discord/Nostr concrete gateways remain dev-only and Web absent; server no-dev tree contains no concrete gateway;
- `cargo fmt --all -- --check`: passed;
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.

No S2+ schema/API, live data, deployment state, or `stash@{0}` was touched.

## P1 follow-up: structural production-authority removal

Base reviewed: `8df88f64be553a6f62d4f4c990228d498e2d126d`.

The user subsequently narrowed this checkpoint to the one structural S1 acceptance failure that
can change the production authority model: the plaintext `OperatorToken` authorizer and
`ExtgateState::new(db, token)` constructor remained compilable under a production Cargo feature.
The other five review observations were explicitly deferred rather than silently implemented.

### Genuine RED

Commit `faef8cf` added the named production-tree assertion before production changes. Command:

```text
cargo test -p opencrab-extgate --test gate_admin_production_boundary \
  production_features_cannot_reach_plaintext_legacy_gate_admin_authorizer -- --exact
```

It failed with exit 101 and:

```text
test production_features_cannot_reach_plaintext_legacy_gate_admin_authorizer ... FAILED
the default production build must not enable test/QC probes
```

The first assertion exposed the then-default feature path; inspection in the same test also pinned
the production `legacy_admin_token` field/constructor and public `OperatorToken` export. Exact output
is retained at `/tmp/issue-1006-s1-p1-red-legacy.txt` for this worktree session.

A malformed-path matrix was also briefly committed RED, proving the reviewed `400` before auth, but
was removed in follow-up commit `5471aa7` when the user explicitly deferred items 1–5. No production
path-extraction change remains in this checkpoint.

### GREEN

Commit `4fd4cdb` removes `bearer.rs`, the public export, legacy state, constructor, authentication and
target-authorization bypasses, and audit skips. `ExtgateState` now has only the database-backed
constructor. Conformance and server QC fixtures use sealed database principals and exact operation,
subject, and instance scopes; probe instrumentation remains available for legitimate QC edges but
no feature compiles a plaintext authorizer. Commits `54f07d3` and `98f4935` make per-instance QC
principals isolated and lock their dev-only base64 helper.

The named production-boundary test is GREEN. The database-backed gate-admin security tests, all 81
extgate conformance tests, 21 Nostr QC tests, and the previously failing multi-instance Discord QC
case are GREEN. Full final validation is recorded in the checkpoint artifact.

### Explicitly deferred Issue candidates (user-prioritized)

1. **Post-bind socket failure cleanup:** trigger is a rare failure after pathname creation during
   listen/fd-transfer/wait/nonblocking/Tokio conversion; impact is a stale UDS pathname requiring
   operator removal before restart. Preserve replacement-inode safety when addressed.
2. **Malformed percent-path auth/audit ordering:** trigger is an invalid percent-encoded route
   parameter; current impact is an Axum `400` before the uniform `401`/sanitized denial audit, with
   no mutation or data exposure observed. Cover all six operations when addressed.
3. **v53 same-name migration collisions:** trigger is an abnormal/manually pre-created same-name S1
   table/index/trigger in a v52 database; impact is potentially retaining a weaker object while
   stamping v53. Add transactional collision fixtures if this state is brought into support scope.
4. **Canonical UUID database checks:** trigger is direct/manual insertion bypassing the manifest
   parser; impact is accepting malformed lowercase 36-byte scope IDs that later fail authorization.
5. **Idempotent audit result classification:** trigger is a successful replay/no-op; impact is
   forensic precision (`succeeded` rather than `idempotent`), not mutation correctness.
