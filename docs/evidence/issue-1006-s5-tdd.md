# Issue #1006 S5 TDD evidence

## Scope

S5 only, starting from independently approved S4 checkpoint
`2995249c921142917876e246755dd4d16b396dd5`. This stage adds concrete-gateway-owned
stores, protected local administration, credential custody, and daemon lifecycle without
migration/cutover, runtime legacy import, S6+ authorization changes, deployment, README edits, or
Issue #1011 work. `stash@{0}` remains untouched.

## Assertion-level RED

The named assertions were run in the working tree directly on the approved S4 checkpoint before
GREEN implementation. The retained raw transcripts contain the compile/test command output and
literal failing assertion:

- `/tmp/issue-1006-s5-discord-red.log`: Discord owner/lifecycle and disabled eligibility tests
  failed with `RED: Discord has no daemon-owned store/admin/lifecycle yet` and
  `RED: Discord has no persisted lifecycle eligibility gate yet`.
- `/tmp/issue-1006-s5-nostr-red.log`: the Nostr persisted-saga assertion failed with
  `RED: Nostr daemon still opens core/legacy DB and lacks the persisted §7 saga`.
- `/tmp/issue-1006-s5-web-red.log`: Web store and local-admin assertions failed with
  `RED: Web has no gateway-owned store yet` and
  `RED: Web has no scoped local admin endpoint yet`.
- `/tmp/issue-1006-s5-supervisor-red.log`: the shared-supervisor assertion failed with
  `RED: concrete platform vocabulary remains in shared supervisor`.

These are genuine pre-implementation failures. The final test names were refined as the concrete
state-machine seams became available; no already-green behavior is represented as RED.

## Minimal GREEN

- Added the platform-neutral `opencrab-process-supervisor` crate, moved generic child ownership out
  of `opencrab-gateway`, retained bounded shutdown/concurrent-reconfigure behavior, and added an
  exclusive durable-store lock. The utility has no concrete-gateway vocabulary or retry policy.
- Discord and Nostr now own SQLite desired/observed lifecycle stores, encrypted credential and
  subject-grant envelopes, exact-scoped mode-`0600` local-admin UDS endpoints, daemon reconciliation,
  child placement, nonce/PID readiness handshakes, crash observation, and durable retry timing.
  Only enabled, verified, generation-exact `ready` rows may start. Persisted `ready` children restart
  while core/server is stopped; disabled/non-ready rows create no child or placement.
- The new generic gate-admin UDS client reads a protected credential file and converges instances
  and deterministic bindings without opening core SQLite. A mock protected-UDS transcript verifies
  bearer use and the complete get/create/bind/verify sequence.
- Nostr production dependencies on `opencrab-core`, `opencrab-db`, `opencrab-gateway`, and
  `opencrab-nostr` were removed. Discord/Nostr daemon configuration denies unknown core/legacy DB
  path fields, and no runtime import/fallback remains.
- Web now owns its SQLite settings/identity/policy/credential store, protected scoped local admin,
  encrypted credential envelopes, and independent owner process. A second owner is refused by the
  store lock; exact reruns converge and agent collisions fail.
- Secrets are absent from placements and admin responses, encrypted at rest (including raw database
  byte scans), removed from daemon/child process environments after intake, and delivered to concrete
  children only through the selected secret environment variable.
- Concrete channel/trusted-user HTTP routes and the unused shared channel-config action were removed.
  The old handlers survive only as whole-file `#![cfg(test)]` support under neutral filenames for
  existing server behavioral tests; production source collection excludes those files.

## Behavioral GREEN

The final focused transcript is `/tmp/issue-1006-s5-focused-green-final.log`:

- Discord library: 68 passed, including fresh provisioning, encrypted restart state, persisted-ready
  server-stopped restart, durable crash retry, disabled/non-ready suppression, scoped admin, and
  runtime core/legacy-path rejection.
- Nostr library: 66 passed with the same owner/lifecycle matrix.
- Web library: 7 passed, including independent server-stopped owner operation, second-owner refusal,
  collision/rerun behavior, store restart, scoped admin, encryption/redaction, and path rejection.
- Process supervisor: 17 passed.
- Gate client: 31 passed, including protected generic UDS reconciliation.

Additional retained validation:

- `/tmp/issue-1006-s5-server-green-final.log`: `CARGO_INCREMENTAL=0 cargo test
  -p opencrab-server --lib --no-fail-fast`, 493 passed. (`CARGO_INCREMENTAL=0` avoided a
  transient local incremental-cache `dep-graph.part.bin` filesystem error from the immediately
  preceding invocation; no source or assertion failure occurred.)
- `/tmp/issue-1006-s5-clippy-final.log`: S5 crates plus server, all targets, `-D warnings`, passed.
- `/tmp/issue-1006-s5-static-final.log`: 61 Python mutation/static tests passed; boundary audit passed
  with 275 classified findings and no unclassified/stale entry.
- `/tmp/issue-1006-s5-no-dev-trees.log`: full `cargo tree --edges no-dev` output for Discord, Nostr,
  Web, and server. `/tmp/issue-1006-s5-no-dev-verify-final.log` is the fresh exact-name verifier:
  no core/shared legacy dependency in any concrete gateway and no concrete gateway dependency in
  server production.
- `cargo check -p opencrab-discord-gateway -p opencrab-nostr-gateway
  -p opencrab-web-gateway --all-targets`: passed.
- `cargo fmt --all -- --check`, `git diff --check`, and the S5 800-line source limit: passed.

Known stale Web all-target/runtime fixtures tracked by Issue #1011 were deliberately not changed and
are not an S5 acceptance gate; no claim is made that those unrelated tests became green.

## Static boundary and baseline

The reviewed baseline falls from S4's 374 findings to 275. The 99 removed findings are S5-owned
concrete server/action/supervisor/Nostr runtime ownership debt. The six remaining gateway SQLite
open sites are exact line/snippet identities classified only as `valid-gateway-owned-store`; moving
or duplicating an open loses that classification. Mutation tests prove that:

- a whole Rust file beginning with `#![cfg(test)]` is excluded from production findings;
- a production file that merely contains a `#[cfg(test)]` module is still audited;
- unknown/moved database opens remain production violations;
- concrete public identity routes and runtime core/legacy path seams remain absent.

Legacy core schema/query findings remain classified for their later authorized stages; S5 deletes no
legacy rows, historical migrations, or production data.

## Rollback and stage boundary

- Code rollback point: approved S4 checkpoint
  `2995249c921142917876e246755dd4d16b396dd5`.
- State rollback inputs for a later cutover remain per-gateway snapshots plus prior independently
  runnable binaries/configuration; this implementation stage performed no cutover or data migration.
- S6 and later remain blocked pending fresh independent S5 approval.

## Final-review P1 fix (follow-up)

The independent review of checkpoint `38bff738276be4a4810b9d6c54c39a91ff26bdb4`
identified five S5 blockers. Assertion-level RED was captured before the follow-up implementation:

- `/tmp/issue-1006-s5-review-lifecycle-red.log`: daemon restart/recovery and readiness-failure
  assertions failed because startup state was normalized without process/core observation.
- `/tmp/issue-1006-s5-review-placement-red.log`: disabled recovery left daemon-owned placement and
  control-socket artifacts.
- `/tmp/issue-1006-s5-review-admin-red.log`: forged request instance scope was trusted by all three
  local-admin implementations.
- `/tmp/issue-1006-s5-review-web-red.log`: Web accepted a non-loopback bind and lacked required
  bearer authentication backed by Web-owned credentials/policy.
- `/tmp/issue-1006-s5-review-config-red.log`: secret-shaped unknown Discord/Nostr instance config
  fields were accepted.

The follow-up GREEN implementation adds daemon-level recovery matrices for every persisted state,
exact-live adoption, stale/lost observation handling, readiness-failure reaping and durable retry,
and per-instance cleanup limited to daemon-owned placement/control paths. Production adoption
requires a live persisted PID plus a placement nonce matching the persisted process nonce; cleanup
never signals a PID without the same ownership proof. Persisted ready/running rows remain restartable
or adoptable while core is temporarily unavailable.

Local-admin instance scope is now supplied only by immutable daemon/owner startup configuration;
request scope must be a member and cannot self-authorize. Discord/Nostr typed configs deny unknown
fields, validate and canonicalize before any durable write, and tests prove secret-shaped rejected
config creates no row. Web settings reject non-loopback binds; the owner decrypts per-instance
Web-owned credentials, loads persisted caller roles, hashes credentials into in-memory bearer
admissions, and applies per-instance authorization to every active HTTP/SSE route.

Retained follow-up GREEN transcripts:

- `/tmp/issue-1006-s5-review-focused-green.log`: Discord 74, Nostr 69, Web 9, process supervisor
  17, and gate client 31 tests passed.
- `/tmp/issue-1006-s5-review-server-green.log`: server library 493 tests passed.
- `/tmp/issue-1006-s5-review-clippy-green.log`: all affected crates plus server, all targets,
  `-D warnings`, formatting, and diff checks passed.
- `/tmp/issue-1006-s5-review-static-green.log`: 61 mutation/static tests and the 275-finding
  reviewed boundary audit passed.
- `/tmp/issue-1006-s5-review-no-dev-green.log`: concrete gateways remain free of forbidden
  core/legacy production dependencies and server remains free of concrete gateway production
  dependencies.

The pre-existing Web integration harnesses remain outside the S5 library gate under Issue #1011;
the new S5 bearer behavior is covered by a focused production-router test for absent, invalid, and
valid credentials, plus owner/store tests for encrypted persistence and caller-policy loading. No
deployment, migration, README, S6+, or Issue #1011
production-harness work was included.
