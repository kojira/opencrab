# Issue #1006 S6 TDD evidence (superseded)

> **Superseded/non-gating (2026-09-25):** All behavior proved by this document—the seven immediate current co-agent relationship/revision rechecks introduced by `f707fe2`, `10bd57e`, and `cffff1d`—is absent at reference `1c3b782` and has been reclassified as category B. Issue #1006 must fully unwind it to historical admitted caller-role snapshot semantics while preserving gateway-owned external identity classification and generic role transport. This evidence is retained only as input to the separate immediate-revocation security follow-up; it is **not** an Issue #1006 execution, QC, release, or deployment gate.

## Scope and design-impact map

Authority is limited to `docs/design-gateway-process-ownership.md` §6 and S6. The review fix changes only the three approved S6 violations at checkpoint `f707fe2d87c824608ebfffcbefb2d58be74127f6`.

| S6 clause | Production seam and behavioral assertion | Preserved boundary |
|---|---|---|
| Initial model turn | `SkillEngine` rejects stale generic relationship evidence before its real `LlmClient`; model-call counter stays zero for revoke and revision bump | No provider/prompt behavior changed |
| Queue dequeue/retry | `SubtaskToolDispatcher` rechecks at its actual dequeue loop; a queued stale item and a concurrently released item both leave the real executor counter at zero | Existing ordering/settlement remains unchanged |
| Tool invocation | `SkillEngine` changes authority after initial admission and rejects before the real `ActionExecutor`; tool counter stays zero | Existing tool policy remains unchanged |
| Automatic continuation | `SkillEngine` changes authority after the first model call and rejects before continuation speech or a second model call | Existing completion and `NO_REPLY` semantics remain unchanged |
| Operation-driven continuation | The actual utterance-operation path passes the generic tool check, then rejects at the operation boundary before executor invocation | S3 metadata remains authoritative |
| Timed/subtask continuation | `s6_actual_process_depth_one_revalidates_before_model_and_tool_effects` drives `run_agent_response` with `depth > 0`, then real DB revoke/revision-bump state; the timed-boundary result is specific and counting model/tool seams both remain zero | S4 generic routing remains unchanged |
| Outbound-delivery commit | All holding, ordinary-continuation, and late-inbound callbacks call `authorize_continuation_speech`, which checks automatic-continuation and then outbound-commit authority immediately before the callback | No S7 ledger/guarantee/reconnect behavior added |

The continuation callback conformance test uses the real extgate `deliver_intermediate_say` transaction. It changes the real DB relationship after the automatic-continuation check returns current and before the outbound-commit check, then proves both `memory_sessions` speech rows and `deliveries` rows remain zero for revoke and revision bump at all three callbacks.

No S7+, Issue #1011, migration/cutover, deployment, README, or unrelated robustness work is included.

## Assertion-level RED

- `/tmp/issue-1006-s6-continuation-callbacks-red.log`: the final callback assertion failed before GREEN because holding continuation speech invoked the callback after revocation (`left: 1`, `right: 0`).
- `/tmp/issue-1006-s6-actual-seams-red.log`: the actual `SkillEngine` seam produced continuation speech after revocation (`revoke:automatic:speech`, `left: 1`, `right: 0`).
- `/tmp/issue-1006-s6-real-ledger-red.log`: mutation removed only the immediate outbound-commit recheck while retaining the automatic check. The final extgate test failed with one real speech row and one real delivery row (`left: (1, 1)`, `right: (0, 0)`).
- `/tmp/issue-1006-s6-actual-process-timed-red.log`: mutation removed the process relationship-authority wiring. The actual `run_agent_response` depth-1 path completed successfully with `model_calls=2` and `tool_calls=1` instead of rejecting at the timed/subtask boundary.

These are behavioral assertion failures at production seams, not compile failures or source-name sentinels. The prior test-only `run_if_current` matrix was removed. The direct `authorize_timed_subtask_entry` unit test remains supplemental and is not acceptance proof.

## Minimal GREEN

- `authorize_continuation_speech` performs the two approved checks immediately before each continuation-speech callback.
- `s6_actual_engine_boundaries_block_model_tool_and_continuation_effects` drives the real model, tool, automatic-continuation, operation, and outbound callback seams for revoke and revision bump.
- `s6_continuation_speech_callbacks_revalidate_after_revoke_and_revision_bump` covers the holding, ordinary, and late-inbound callback branches.
- `s6_real_queue_dequeue_and_concurrent_release_revalidate_current_relationship` drives the production dispatcher dequeue loop for both queued stale work and deterministic release-after-mutation.
- `s6_actual_process_depth_one_revalidates_before_model_and_tool_effects` exercises the actual `run_agent_response` production seam with `depth > 0`, real DB revoke and revision bump, and zero model/tool effects. Its boundary-specific rejection also fails if `process` stops invoking the timed/subtask check.
- `s6_real_timed_subtask_entry_rejects_revoke_and_revision_bump_before_model` remains supplemental unit coverage for the fixed-boundary helper.
- `s6_three_real_continuation_callbacks_leave_speech_and_delivery_ledgers_empty` proves zero real speech/delivery ledger rows at all three callbacks.

## Retained GREEN transcripts

- `/tmp/issue-1006-s6-actual-seams-green.log`: focused production-seam tests passed: core 2, actions queue/concurrency 1, server helper 1, extgate real-ledger 1.
- `/tmp/issue-1006-s6-actual-process-timed-green.log`: actual process depth-1 timed/subtask test passed for real DB revoke and revision bump, with zero model/tool calls.
- `/tmp/issue-1006-s6-file-size-green.log`: exact `bash scripts/check-file-size.sh` rerun passed (`OK: every Rust source file is at most 800 lines.`).
- `/tmp/issue-1006-s6-design-fix-clippy.log`: focused affected-crate clippy passed with warnings denied.
- `/tmp/issue-1006-s6-design-fix-static.log`: formatting, dependency/static audit, and boundary mutation tests passed.

The previously retained `/tmp/issue-1006-s6-static-green.log` is not GREEN evidence for file size; it contains the superseded failure and is intentionally not cited as passing evidence.
