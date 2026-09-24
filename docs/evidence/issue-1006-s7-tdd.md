# Issue #1006 S7 TDD evidence

Base: `cffff1dbdee3e04b654ded8866020b9ac8b882bc`.
Authority: `docs/design-gateway-process-ownership.md` S7 and its referenced §11 delivery contract only.

## Fixed acceptance checklist

| Design clause | Production seam | Behavioral RED | Minimal GREEN | Passing evidence | Forbidden/later-stage impact |
|---|---|---|---|---|---|
| Core pending/ack row has immutable guarantee, payload digest, prepared protocol evidence | extgate `deliveries` schema/query and say send/response/reconnect | `s7_core_delivery_ledger_*` migration/runtime assertions | v57 generic columns, immutable conflict checks, ordered pending replay/ack | retained focused extgate/db logs | no platform field; no S8 migration/cutover |
| Gateway emission row keyed `(binding_id, delivery_id)` | Discord/Nostr owned stores and live say consumers | `s7_*_emission_ledger_*` | durable prepared/terminal row and exact replay/conflict checks | retained gateway focused logs | no core DB open; no fabricated legacy receipts |
| Every prepare/send/receipt/ack crash window | extgate delivery runtime plus concrete store state machines | table-driven `s7_*_crash_window_matrix` | persist-before-I/O, receipt-before-core response, ack-before retention | deterministic fault matrix log | never early core ack |
| same key/digest/guarantee replay; digest/guarantee conflict | core and both gateway stores | exact replay/conflict assertions | byte-idempotent verify-or-create; hard conflict | ledger digest test log | never relabel guarantee |
| ordered reconnect drain; close with pending; retention handshake | extgate hello/close and gateway stores | ordered replay and retention high-water assertions | generic reconnect frames; ack permits pruning only after terminal | extgate/gateway logs | no reroute/resend of terminal or unresolved rows |
| weaker/same/stronger hello and recognized/unrecognized adapter protocol | extgate compatibility and gateway prepared-row resume | matrix assertions | persisted guarantee controls; stronger may satisfy weaker without relabel; weaker/unknown blocks I/O | compatibility matrix log | no universal exactly-once claim |
| terminal rows perform no external I/O | Discord/Nostr consumers and store state machine | terminal replay counters remain zero | return durable outcome directly | gateway log | never resend indeterminate/operator-blocked |
| Nostr one persisted signed event ID/bytes across retries | Nostr store/delivery seam | `s7_nostr_signs_once_*` | prepare deterministic signed request once; retry exact persisted bytes/event ID | Nostr logical-exactly-once log | exactly-once means logical event identity only |
| Discord stable <=25 nonce, bounded `enforce_nonce`, persisted reference, ambiguity expiry no resend | Discord store/transport seam | `s7_discord_nonce_window_*` | persisted nonce/request; bounded retry; durable indeterminate after recovery/expiry | Discord nonce-window log | Discord never advertises exactly-once |
| S6 pre-commit revocation remains effective | existing S6 production-seam tests | retained S6 matrix | no change except delivery path remains after authority check | focused S6 tests | no cached authorization or post-revoke ledger commit |

No checklist item may be added by implementation or review. S8+, migration/cutover/deployment, README, Issue #1011, and robustness outside these clauses are excluded.

## RED

Pending.

## GREEN

Pending.
