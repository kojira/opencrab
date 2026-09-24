# Issue #1006 S4 TDD evidence

## Scope

S4 only, starting from independently approved S3 checkpoint `097f8aee21fc28e97dd419a2fe8e76c1fc524ab0`.
No S5+ gateway-owned store/lifecycle work, offline import, runtime fallback, legacy deletion,
Issue #1011 changes, deployment, or README edits are included.

## Assertion-level RED

- DB schema/query/projection: `/tmp/issue-1006-s4-db-red.log` failed with the named
  `SessionHeartbeatInstructionsRow`, composite resolver, precedence, stopped projection,
  conflict, and fingerprint APIs absent.
- Protected gate-admin boundary: `/tmp/issue-1006-s4-gate-admin-red.log` failed because a
  `PUT binding` containing `channel_id` returned `201` instead of `400`, proving the generic
  envelope silently accepted a platform destination field.
- Scheduler transcript: `/tmp/issue-1006-s4-scheduler-red.log` replays the final schema and
  assertion on checkpoint `097f8ae` while retaining the pre-GREEN heartbeat fire seam. The real
  scheduler delivered a request, but the prompt assertion failed because the session override was
  ignored.
- Static boundary: `/tmp/issue-1006-s4-static-red.log` ran the final live-path assertion against
  checkpoint `097f8ae` and found 25 platform-destination occurrences in the heartbeat runtime.
- The updated legacy QC harness was also invoked at the RED checkpoint; it stopped at the known
  unrelated Issue #1011 binding-ack harness failure before reaching the heartbeat assertion
  (`/tmp/issue-1006-s4-qc-red.log`). The deterministic production scheduler transcript above is
  the executable S4 heartbeat acceptance seam; no unrelated harness repair was made.

## Minimal GREEN

- Schema v55 and fresh schema create `session_heartbeat_instructions` with composite
  `(agent_id, session_id)` primary key, composite FK to `session_heartbeat_config`, and session FK.
- Generic query APIs read/write/resolve nullable session overrides. `NULL` resolves the current
  agent instructions, then the generic default.
- Exact/global projection resolution chooses config and instructions independently: exact config
  shadows global config; an empty exact instruction falls back to a non-empty global instruction.
- The stopped projection API creates an absent config/instruction pair, accepts only an exact
  retry, and refuses partial/non-identical pre-existing targets. Its initial fingerprint includes
  `last_fired_at`; its long-lived lineage digest excludes `last_fired_at` and update timestamps.
- Live scheduler dispatch resolves instructions by generic `(agent_id, session_id)` and emits only
  canonical `binding_id`/`session_id` work. Successful fire still advances `last_fired_at`.
- The heartbeat tools now use `agent | session | effective` and `session_id`; live shared code no
  longer administers channel/guild heartbeat destinations. The six-operation gate-admin contract
  is preserved; `PUT binding` now rejects unknown/platform destination fields without partial
  writes.
- Existing `session_watches` historical/source schema remains intact for guarded S10 cleanup, but
  S4 live heartbeat files are statically required to contain no platform destination or
  `session_watches` runtime seam.

## Focused GREEN evidence

- `cargo test -p opencrab-db s4_ -- --nocapture`: 5 passed.
- `cargo test -p opencrab-db --all-targets --no-fail-fast`: 267 passed, 3 ignored.
- `cargo test -p opencrab-server --bin opencrab-server s4_scheduler_emits_generic_binding_session_with_session_instructions_and_advances_anchor -- --nocapture`: 1 passed.
- `cargo test -p opencrab-server heartbeat_instructions --lib -- --nocapture`: 7 passed.
- `cargo test -p opencrab-extgate --test conformance s4_gate_admin_binding_rejects_platform_destination_fields_without_partial_targets -- --nocapture`: 1 passed.
- `python3 -m unittest scripts/tests/test_gateway_boundary_audit.py`: 56 passed.
- `python3 scripts/gateway_boundary_audit.py`: 374 classified findings, no unclassified/stale entry.

The production-shaped Discord/Nostr QC heartbeat fixtures now seed the generic composite
instruction row. A direct run of the Discord fixture still stops at the pre-existing Issue #1011
`binding が ack されない` harness failure before heartbeat execution; this stage deliberately does
not patch that unrelated harness.

## Boundary burn-down

The reviewed baseline moves from 412 to 374 findings. The 38 removed findings are the concrete
channel/guild administration and audit vocabulary eliminated from the live shared heartbeat tool
and definition paths. Legacy source schema/query findings remain classified for S8/S10; no source
row or historical migration was deleted.
