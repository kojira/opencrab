# Issue #1006 S6 TDD evidence

## Scope and design-impact map

Authority is limited to `docs/design-gateway-process-ownership.md` §6 and S6. Before editing, each change was mapped as follows:

| S6 clause | Minimal change | Preserved boundary |
|---|---|---|
| Gateway supplies generic co-agent ID plus relationship revision | `RelationshipAuthority`; V3 `co_agent` caller requires positive `relationship_revision`; Discord/Nostr gateway-owned access projections carry the generic pair | Gateways still authenticate external IDs; core receives no platform identity or policy |
| Core owns current internal relationship/revision | v56 adds `relationship_revision` and `active` to existing internal `trusted_co_agents`; revoke and bump invalidate older evidence | No gateway DB is opened by core; S0–S5 store/lifecycle ownership is unchanged |
| Initial model turn | SkillEngine revalidates immediately before the first LLM request | No model/provider or prompt behavior changed |
| Queue dequeue/retry | extgate dequeue and auto-dispatch dequeue revalidate before work executes | Existing session locking/queue ordering remains unchanged |
| Tool invocation | SkillEngine revalidates before inline or dispatched execution | Existing tool policy and dispatch classification remain unchanged |
| Automatic continuation | every later LLM iteration and continuation-speech callback revalidate | Existing completion and `NO_REPLY` semantics remain unchanged |
| Operation-driven continuation | utterance operation revalidates before executor invocation | S3 dynamic metadata remains authoritative; no operation-name policy was added |
| Timed/subtask continuation | inherited manual-subtask authority is revalidated on sub-run; extgate completion sink revalidates before resume | S4 generic binding/session routing remains unchanged |
| Outbound-delivery commit | extgate checks current authority immediately before final delivery effect; revoked work becomes empty and creates no outbound row | No S7 emission ledger, guarantee, reconnect, or adapter work was added |

No S7+, Issue #1011, migration/cutover, deployment, README, crash-window, malformed-internal-input, or unrelated robustness work is included.

## Assertion-level RED

The final table-driven seven-boundary test was replayed with the current-authority guard deliberately removed. `/tmp/issue-1006-s6-seven-boundary-red.log` failed and listed all fourteen unwanted side effects:

- revoke at all seven boundaries;
- revision bump at all seven boundaries.

This was an assertion failure, not a compile failure. The production guard was then restored.

## Minimal GREEN

- `AuthorizationBoundary::ALL` is exactly the seven approved boundaries.
- `s6_seven_boundaries_fail_closed_after_revoke_and_revision_bump` proves zero side effects for revoke and revision bump at every boundary.
- `s6_queued_and_concurrent_boundaries_never_use_cached_authority` releases queued workers only after a revision bump and proves zero side effects, so the current DB relationship is read at execution time rather than cached at admission.
- v56 fresh/upgrade parity and the current/revoke/bump queries pass.
- V3 rejects a co-agent caller without a positive relationship revision.

## Retained GREEN transcripts

- `/tmp/issue-1006-s6-matrix-green.log`: focused seven-boundary and concurrent matrix passed.
- `/tmp/issue-1006-s6-focused-green.log`: final matrix 2 passed, subtask/dispatcher 56 passed, V3 revision parser 1 passed.
- `/tmp/issue-1006-s6-db-green.log`: DB all-targets 268 passed, 3 ignored.
- `/tmp/issue-1006-s6-extgate-green.log`: extgate 72 library, 91 conformance, 1 production-boundary, 8 no-platform, and 3 S3 static tests passed.
- `/tmp/issue-1006-s6-server-green.log`: server library 495 passed.
- `/tmp/issue-1006-s6-gateways-green.log`: Discord 74, Nostr 69, gate-client 31 + 7 passed.
- `/tmp/issue-1006-s6-clippy-green.log`: workspace all-target/all-feature clippy passed with warnings denied.
- `/tmp/issue-1006-s6-static-green.log`: actions tests passed; dependency checks passed. File-size was rerun after extracting authorization helpers and now passes at exactly 800 lines maximum.

The reviewed boundary baseline remains 275 findings; only exact line identities moved. `gateway_boundary_audit.py` and its 61 mutation tests pass.
