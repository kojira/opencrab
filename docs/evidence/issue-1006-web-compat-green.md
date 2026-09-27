# D-1006-WEB-01 focused GREEN

Forward assertion-level RED is recorded at `6185c1c` in [issue-1006-web-compat-red.md](issue-1006-web-compat-red.md). This checkpoint restores only historical Web admission and stopped Web identity migration; it is **not** full Issue #1006, S9 live QC, or S10 cleanup acceptance.

| Focused validation | Result |
| --- | --- |
| `cargo test -p opencrab-web-gateway --lib --test rust-unit --test web_owner_compat -- --test-threads=1` | 9 + 16 + 1 pass; existing routes work without a bearer, admitted frame keeps `Owner` and configured author, owner starts without a Web key/credential. |
| `cargo test -p opencrab-gateway-migrate -- --test-threads=1` | 14 + 4 + 5 pass; explicitly mapped Web co-agent/multiple/unrelated role rows remain non-admission projections, existing Web envelope remains unchanged, a NULL envelope remains NULL, ordinary rerun passes, and a matched core+Web snapshot set restores on disposable paths. Discord/Nostr tests stay green. |
| `cargo fmt --all -- --check`, `bash scripts/check-file-size.sh`, `git diff --check` | Pass. |

Existing Web process/conformance tests **are not GREEN**: `--test conformance` fails because an old fixture expects protocol 2 while the current generic hello emits 3; `--test core_process_e2e`, `--test web_conversation_create_e2e`, and `--test web_mock_contracts_e2e` cannot start the already-modified core service because their fixture config lacks the required `[gate_admin]` credential file. The fixture protocol expectation and missing core configuration are unchanged by D-W1–3; these tests were not run at `6185c1c`, so no retrospective RED is claimed. They are tracked as stale Web fixtures outside this behavior correction; no success is inferred from them. No live QC, real DB, merge, or deployment was performed.

S8 retains original `trusted_users` source rows. Existing Web projections do not preserve every legacy row ID/metadata field, so S10 identity deletion remains blocked pending its separate lossless disposition proof.
