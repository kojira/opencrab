# D-1006-CRED-01 forward RED (HEAD 1c15377)

Only tests and design were added. Production source, real QC/production data, and existing S5 schema were not changed.

Command: `cargo test -p opencrab-gateway-migrate --test s8 s8_disabled_ -- --nocapture`

- `s8_disabled_discord_without_credential_imports_and_reruns_unconfigured` — **FAILED** at `tests/s8.rs:527`: `credential source missing` before the stopped import; assertion expects the disabled instance to import without inventing a credential, preserve `enabled=false` and `credential_envelope=''`, report zero credentials, and accept successful rerun.
- `s8_disabled_nostr_without_credential_imports_and_reruns_unconfigured` — **FAILED** at the same assertion/message on the corresponding Nostr fixture.
- Result: 0 passed, 2 failed; these are RED, **not** successful migration evidence. The assertions after import have not executed and must not be called GREEN.

Control command: `cargo test -p opencrab-gateway-migrate --test s8 s8_enabled_discord_without_credential_still_fails_closed -- --exact` — **PASSED** (1/1); existing enabled-without-source rejection remains in place. Both fixtures use temporary v56 core and S5 gateway databases, preserve an open generic binding, and pass no secret descriptor; no live data is read.

Minimal next change: select an explicit absent-credential state only for disabled instances, accept/insert the S5 empty envelope without encrypt/decrypt, omit configured credential evidence, and compare that same state on a successful rerun. Preserve existing nonempty envelopes and enabled fail-closed behavior.
