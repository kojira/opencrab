# D-1006-CRED-01 GREEN

Forward RED: `docs/evidence/issue-1006-disabled-credential-red.md` at `202c324` (two disabled imports failed `credential source missing`; enabled rejection passed).

After the production change, `cargo test -p opencrab-gateway-migrate --test s8 s8_disabled_ -- --nocapture` passed 2/2. The disabled Discord and Nostr fixtures import with unchanged instance IDs, `enabled=false`, an empty S5 credential envelope, no configured-credential report entry, and an idempotent rerun. `cargo test -p opencrab-gateway-migrate --test s8 s8_enabled_discord_without_credential_still_fails_closed -- --exact` passed 1/1: missing enabled credential fails before backup.

Focused regression: `cargo test -p opencrab-gateway-migrate --test s8` passed 7/7, `--test s8_web` passed 5/5, `cargo test -p opencrab-web-gateway --test web_owner_compat` passed 1/1. `cargo fmt --all -- --check`, `bash scripts/check-file-size.sh`, and `git diff --check` passed.

This change does not supply a credential to an enabled instance with no authoritative source. It changes no S5 live admin policy, real QC/production data, or S10 cleanup.
