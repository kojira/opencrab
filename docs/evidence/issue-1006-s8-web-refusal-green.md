# S8 Web identity refusal: minimal GREEN

Forward assertion-level RED is recorded in [issue-1006-s8-web-refusal-red.md](issue-1006-s8-web-refusal-red.md) at `262db906`; the co-agent refusal was already passing there. The only production change is read-only Web identity prevalidation in `destination::prevalidate`, which runs before the matched backup. It limits approved mapped source rows to one per Web instance and requires that the destination have exactly one `external_id='bearer'` role equal to that source role. This also rejects a literal source `user_id='bearer'` with a conflicting bearer role before backup, without altering the persisted bearer. It does not change Web credential, author, runtime admission, or non-Web paths.

Validation on disposable schema-56/S5 stores:

| Command/check | Result |
| --- | --- |
| `cargo test -p opencrab-gateway-migrate --test s8_web s8_web_rejects -- --nocapture` | 4 passed: co-agent, two source rows, mismatched role, literal bearer collision; all require no backup/report and unchanged stores |
| `cargo test -p opencrab-gateway-migrate --test s8_web s8_web_existing_bearer_preserves_independent_source_identity_and_installs_credential -- --nocapture` | 1 passed; distinct external identity, existing bearer, Web credential install, and rerun |
| `cargo test -p opencrab-gateway-migrate --test s8 -- --nocapture` | 3 passed |
| `cargo fmt --all -- --check`; `git diff --check`; `bash scripts/check-file-size.sh` | all passed; largest changed file 736 lines (800-line limit) |

This is focused local migration evidence, not isolated S9 QC, real-data cutover, main merge, or deployment approval.
