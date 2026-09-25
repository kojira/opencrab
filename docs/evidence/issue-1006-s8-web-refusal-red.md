# S8 Web identity refusal: forward RED at 262db906

Scope: schema-56 disposable core, valid S5 WebStore with an existing encrypted credential and one persisted `trusted_user` bearer role. Every source row has an explicit approved edge to that existing instance. The existing single-row happy path is independently GREEN in `s8_web_existing_bearer_preserves_independent_source_identity_and_installs_credential`; the refusal fixtures use the same core/Web author and an existing destination credential, not a missing-file or NULL-envelope shortcut.

Command: `cargo test -p opencrab-gateway-migrate --test s8_web s8_web_rejects -- --nocapture`.

| Assertion | Observed result at 262db906 | Status |
| --- | --- | --- |
| Single `co-agent` source must fail before backup | Passes with `identity is not represented by gateway config` | Already GREEN; no forward RED claimed |
| Two distinct mapped `trusted_user` source rows targeting the same Web bearer must fail before backup | `unapproved Web identity mapping was accepted` | Genuine assertion-level RED |
| Single `owner` source with an existing `trusted_user` bearer must fail before backup | `unapproved Web identity mapping was accepted` | Genuine assertion-level RED |
| Source `user_id='bearer'` with `owner` role conflicts with existing `trusted_user` bearer and must fail before backup | Tool returns `Web bearer caller role must be unique` **after** creating the matched backup; pre-backup assertion fails | Genuine timing RED, not a successful admission refusal; no before-write claim |

The targeted run reports 1 passed, 3 failed. The test assertions require rejection before matched backup and unchanged source/destination, not only a late database constraint. No production source, main branch, runtime, or production database changed. These RED tests must not be represented as GREEN or as an approved S8 gate.
