# Issue #1006 strict behavior-preserving separation redesign

## Decision and evidence baseline

Owner direction on 2026-09-25 narrows Issue #1006 to separation only. The reference behavior is commit `1c3b7821a46ad1dfdacfd5640cd8943cc211dc41`; the last approved pre-S4 checkpoint is `097f8aee21fc28e97dd419a2fe8e76c1fc524ab0`; the paused branch is `f25d51af849ea8b80984103c32a669a5bbb3fa19`.

At `1c3b782` delivery already had one core-owned `deliveries` ledger with states `sending`, `delivered`, `failed`, and `indeterminate`. `send_text` committed the speech row and `sending` row before writing one runtime `say` frame. A write failure or live disconnect changed in-memory pending sends to terminal `indeterminate`; startup changed stale `sending` rows to terminal `indeterminate`. It did not replay them. Discord sent message chunks sequentially once, failed fast, and did not persist a nonce or external request. Nostr invoked one `nostaro post` and did not persist signed event bytes or replay identity. No production source contained `delivery_guarantee`, `required_delivery_guarantee`, prepared-protocol evidence, a gateway emission ledger, `operator_blocked`, a retention high-water handshake, or capability upgrade/downgrade recovery.

Strict parity therefore means moving the existing generic runtime frame across the new owner/process boundary without changing these observable outcomes. A disconnect remains `indeterminate`; stale sends remain `indeterminate`; no automatic reconnect resend is introduced. This intentionally does not close the external send-before-receipt ambiguity.

## Classification

### A — required separation while preserving behavior

| Stage/commit | Files or seam | Why retained |
|---|---|---|
| S3 `2f0ad32`, `2afe535`, `61206d9`, `097f8ae` after selective unwind | extgate/gate-client dynamic declarations, generic binding/session routing, platform-neutral timed fire | Removes operation-name and platform routing knowledge from shared/server. Retain `authorization`, `dispatch`, `sub_engine`, `sharing`, `effect`, `final_delivery`, declaration digest, opaque binding/session IDs, and exact/global generic routing. Remove only delivery-guarantee negotiation listed under B. |
| S4 `581a138`, `2995249` | heartbeat schema/query/router and scheduler paths | Generic heartbeat configuration and exact/global projection replace platform-shaped watches in core. This is ownership separation, not an external-delivery guarantee. Retain S4. |
| S5 `38bff73`, `1e80eac` | Discord/Nostr/Web stores, local admin, encrypted credentials, daemon child lifecycle, process-supervisor utility | These changes establish the required platform state, credential, identity/policy, and lifecycle owners. S5 gateway schemas contain no external-emission ledger at the S5 checkpoint. Retain S5. |
| S6 `f707fe2`, `10bd57e`, `cffff1d` | generic co-agent relationship/revision authority and boundary checks | Retain as a security requirement created by moving external identity classification to gateways. It is not a delivery-retry guarantee. The outbound check remains before the existing core `deliveries` insert/send seam. |
| replacement S7 | extgate runtime `say`/utterance frame, gate-client handlers, concrete send adapters | Preserve the pre-separation one-attempt mapping: core writes the existing `sending` row, gateway performs the existing send once, response maps to existing terminal state, disconnect/startup ambiguity maps to `indeterminate`, and no concrete platform vocabulary enters shared/server. |

### B — new behavior absent before separation; unwind required

| Origin | Exact source/schema/API | Safe unwind and impact |
|---|---|---|
| S3 `2f0ad32`, `2afe535` | `DeliveryGuarantee`, hello `delivery_guarantee`, declaration/invocation `required_delivery_guarantee`, guarantee fields in declaration digest/invoke frames and live registry. Files: `crates/extgate/src/{lib.rs,operations.rs,protocol.rs,registry.rs,listen/hello.rs,operation_calls.rs}`, `crates/gate-client/src/{wire.rs,lib.rs,client/state_api.rs}`, Discord/Nostr `run.rs`, and their tests. | Selectively remove guarantee enums/fields/validation/tests while preserving dynamic operation metadata and declaration digest. Invocation remains fail-closed on declaration digest/dispatch/effect drift, not on a new delivery guarantee. This is an intentional wire revision requiring matched core/gateway binaries at cutover; no compatibility fallback. |
| S7 `f25d51a` schema | migration v57 adds `payload_digest`, `delivery_guarantee`, `prepared_protocol_digest`, `acknowledged_at`, `frame_kind`, `prepared_frame_json`, immutable trigger, and pending replay index to core `deliveries`. Files: `crates/db/src/schema/migrations/v57.rs`, migration registration/tests. | Revert v57 completely. It has not been deployed from this branch. Core retains the pre-existing delivery table/states and startup recovery. Later migration numbers must be renumbered only if necessary before release; no empty compatibility migration is needed for an undeployed branch. |
| S7 `f25d51a` gateway ledger | `crates/gate-client/src/emission.rs` and delivery wire/API modules create `emission_ledger`, `EmissionState::{Prepared,Receipted,Failed,Indeterminate,OperatorBlocked}`, immutable request material, external-attempt state, and core acknowledgement. | Revert these files/exports/dependencies completely. S5 stores remain owners of config/identity/policy/credentials/lifecycle but gain no second external-emission ledger in Issue #1006. |
| S7 `f25d51a` core replay | extgate persists prepared `say`/`invoke` frames, leaves close/startup rows pending, drains them on bind reconnect, sends `delivery_ack`, and records `acknowledged_at`. Files: `crates/extgate/src/{delivery.rs,close.rs,listen/mod.rs,listen/response.rs,operation_calls.rs,protocol.rs}`. | Revert to pre-S7 behavior: close/startup terminalize ambiguous `sending` rows as `indeterminate`; no ordered reconnect replay or retention acknowledgement. This restores observable parity but does not provide automatic recovery. |
| S7 `f25d51a` Discord strengthening | deterministic per-delivery/per-chunk nonce, `enforce_nonce`, persisted prepared chunk sequence/reference, ledger-driven resume. Files: `crates/discord-gateway/src/{post.rs,run.rs,transport.rs,config.rs,main.rs,daemon.rs}` plus tests. | Revert S7 nonce/ledger path. Preserve pre-S7 sequential chunking, fail-fast, one attempt, and returned last message ID/reaction behavior. |
| S7 `f25d51a` Nostr strengthening | gateway emission preparation/resume and delivery module intended to persist/reuse one event identity. Files: `crates/nostr-gateway/src/{post.rs,run.rs,run/delivery.rs,config.rs,main.rs,daemon.rs}` plus dependencies. | Revert S7 module/API. Preserve the existing single `nostaro post`/reply behavior and result mapping. Do not claim logical exactly-once. |
| Design-only migration additions | legacy guarantee classes (`legacy_unqualified`, `exactly_once`, `at_most_once_indeterminate`), migration-created gateway tombstones, split-ledger completeness/retention digests. | Remove from import/projection/freeze/cleanup. Preserve the existing core `deliveries` rows byte-for-byte and include that single table in core snapshot/digest verification. A pre-cutover `sending` row is handled by the already-existing startup transition to `indeterminate`, not reconstructed or replayed. |
| Design-only recovery promises | crash-window closure, weaker/same/stronger hello matrix, prepared-protocol compatibility, retention high-water, Nostr same-event replay, Discord bounded retry/no-resend guarantee labels. | Remove from S7/S9/S11 and release criteria. Keep only parity assertions for one attempt and existing terminal mapping. These stronger guarantees may be proposed in a separate issue with their own design. |

The safest implementation unwind starts with a full revert of `f25d51a`, because that checkpoint is wholly S7 strengthening and has no approved downstream code dependency. Then selectively remove the S3 guarantee fields without reverting dynamic metadata/routing. S4, S5, and S6 remain; their evidence is rerun after the unwind. Do not revert S4–S6 wholesale.

### C — tests/evidence only

- `docs/evidence/issue-1006-s7-tdd.md` describes the superseded two-ledger scope and must not be used as an execution gate after this redesign.
- v57 migration tests, S7 emission/reconnect/nonce tests, and guarantee-specific portions of S3 operation/wire tests are removed or rewritten because they assert B behavior.
- S4 heartbeat, S5 ownership/lifecycle, and S6 authorization evidence remains valid but must be rerun after the unwind.
- Replacement S7 evidence must compare observable outcomes with `1c3b782`: one core row, one external attempt, the same delivered/failed/indeterminate mapping, sequential Discord chunks, one Nostr command, disconnect terminalization, and startup stale-send terminalization.

## Ready-to-file follow-up GitHub Issue

Do not file this from the design-correction task. The parent session will review and file it. Copy the title and body exactly unless review finds an evidence error.

**Title**

`Design and implement crash-safe external-delivery durability after gateway separation`

**Body**

```markdown
## Context

Issue #1006 is intentionally limited to strict behavior-preserving gateway ownership/process/storage separation. Its reference behavior is commit `1c3b7821a46ad1dfdacfd5640cd8943cc211dc41`: one core `deliveries` ledger; one external send attempt; disconnect/startup ambiguity becomes terminal `indeterminate`; no automatic reconnect replay; Discord sequential fail-fast chunks; one Nostr post/reply command attempt.

During #1006 S3/S7, a stronger two-ledger external-delivery design was proposed and partially implemented. Owner direction removed every enhancement absent before separation from #1006. Evidence and the unwind map are in `docs/evidence/issue-1006-strict-separation-redesign.md` at commit `6faf26eef425fbd17400dc9d49a4c5ca66682405` (plus the follow-up clarification commit that files this issue text).

## Deferred scope

Design as one coherent protocol, then implement only after separate approval:

- a gateway-owned durable emission ledger keyed by `(binding_id, delivery_id)`;
- immutable payload/request identity and prepared request material;
- adapter protocol/capability digest and compatibility rules;
- explicit external-delivery guarantee labels and declaration/invocation negotiation;
- core receipt acknowledgement and gateway retention/high-water handshake;
- ordered reconnect drain and all prepare/send/receipt/ack crash windows;
- terminal outcome replay without external I/O;
- downgrade/upgrade/unknown-protocol behavior without silent relabeling;
- Discord persisted per-delivery/per-chunk nonce, bounded `enforce_nonce`, external reference persistence, and durable ambiguity policy;
- Nostr persist/sign-once event bytes and same-event-ID reconciliation/republication;
- migration rules for existing terminal, stale `sending`, and ambiguous rows without fabricated receipts;
- matched core/gateway snapshot, rollback, QC, and retention evidence.

## Existing partial evidence to reuse only as design input

- superseded checkpoint `f25d51af849ea8b80984103c32a669a5bbb3fa19`;
- deferred uncommitted patch `issue-1006-s7-two-ledger-deferred-20260925.patch`, SHA-256 `de260afa4009a31627dbfaa4bb5aa3bb941faae405bedc23ed84ce541e4a1be9`;
- original S3 guarantee additions in `2f0ad32` and `2afe535`;
- proposed v57 delivery-evidence schema, `gate-client::emission`, reconnect replay, Discord nonce, and Nostr delivery modules inventoried in the redesign evidence.

These artifacts are not approved implementation and must not be applied wholesale. Obtain assertion-level RED before new production edits.

## Dependencies

- #1006 ownership/process/storage separation is completed and independently accepted first.
- The stable post-#1006 generic delivery parity protocol and schemas are the design baseline.
- A new design ID/architecture approval is required because this changes product behavior, wire protocol, schemas, migration, and external-delivery guarantees.
- Deployment tooling must validate migrations on clones and preserve matched core plus all participating gateway snapshots.

## Acceptance direction

The future design must state the exact guarantee for each adapter, every durable state/transition, all crash windows, reconnect compatibility, migration semantics, retention authorization, and rollback set. It must not call external delivery exactly-once unless the adapter primitive closes ambiguity for the entire supported recovery period. It must preserve the zero-core-change new-gateway rule by using platform-neutral protocol capabilities rather than concrete kind branches.

## Explicit non-gate statement

**This follow-up is not a completion, merge, release, QC, or deployment gate for Issue #1006.** #1006 is complete when strict behavior-preserving separation and its own S0–S11 evidence pass. This issue begins only after #1006 acceptance and cannot be used to expand #1006 scope retroactively.
```

## Revised stages

1. **S3 correction:** assertion-level RED that dynamic metadata/routing still works without guarantee fields; minimal GREEN removes guarantee negotiation only. Rerun all retained S3 routing/operation tests and S0 static audits.
2. **S4 retained/revalidation:** no production change unless regression appears; rerun approved heartbeat evidence.
3. **S5 retained/revalidation:** no second emission ledger; rerun store/admin/credential/lifecycle and no-core-open evidence.
4. **S6 retained/revalidation:** keep security checks; rerun seven boundaries against the original single core delivery insert/send seam.
5. **S7 replacement — generic delivery parity:** RED/GREEN at real extgate/gate-client/Discord/Nostr seams proving exact pre-separation outcome mapping and no platform knowledge. No schema migration, gateway emission ledger, resend/reconcile, guarantee label, prepared digest, nonce strengthening, or retention handshake.
6. **S8 migration:** import platform state only; preserve the single core delivery ledger byte-for-byte; remove guarantee classification and split-ledger manifest requirements.
7. **S9 QC:** prove identity/history/ID continuity, server-stopped gateway ownership, dynamic operations, heartbeat, S6 security, and delivery parity. Synthetic gateway still proves zero core/shared/server source/schema/migration/redeployment changes.
8. **S10 cleanup:** preserve the core delivery table and digest; no gateway emission ledger exists to migrate or prune.
9. **S11 readiness:** matched rollback and all separation/security/migration gates; no stronger external-delivery claim.

## Replacement S7 checklist

- Core uses the existing `deliveries(delivery_id,binding_id,payload_json,state,error,created_at,updated_at)` shape and existing state transitions only.
- Speech/session log plus `sending` insert retains the existing transaction boundary.
- A live acknowledged binding receives exactly one generic `say` or utterance invoke frame per attempt.
- Gateway maps existing success, permanent rejection, and unknown/disconnect outcomes to the same core terminal states as `1c3b782`.
- Connection close and startup stale-send recovery end ambiguous rows as `indeterminate`; they do not replay.
- Discord preserves sequential chunking, fail-fast, no automatic retry, last-message reference/reaction behavior, and existing long-message output.
- Nostr preserves one existing post/reply command attempt and current failure mapping; no persisted signed-event replay.
- Automatic versus operation-driven final delivery remains metadata-driven and no operation-name allowlist returns.
- Synthetic gateway sends through the same generic frame without core/shared/server changes or a concrete kind branch.
- S6 revalidation still occurs immediately before the existing delivery insert/send seam.

## Explicit conflicts and decisions

- Strict parity cannot coexist with the former claim that process separation also closes every external-delivery crash window. This redesign chooses parity and removes that claim.
- Strict parity cannot coexist with the former `exactly_once`/`at_most_once_indeterminate` negotiation API. This redesign removes it rather than leaving unused labels.
- Zero-core-change extensibility does not require negotiated external-delivery guarantees. It requires a platform-neutral frame and opaque IDs; the replacement S7 satisfies that weaker contract.
- S6 immediate co-agent revocation is stricter than the historical snapshot behavior. It remains because moving external identity authority out of core without current relationship validation would weaken security. If the owner requires literal parity even when it weakens revocation, that is a separate explicit security decision; this redesign does not make it.
- No owner decision is required to remove the superseded two-ledger work: the owner already directed separation-only. A future stronger-delivery issue requires a new design and approval.
