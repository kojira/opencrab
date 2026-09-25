# D-1006-ID-01 — stopped identity retention GREEN

Starting from assertion-level RED `898b18e` (`docs/evidence/issue-1006-identity-retention-red.md`), S8 now copies each explicitly approved gateway identity edge's original eight `trusted_users` TEXT values into its destination's inert `legacy_identity_sources` record keyed by `(instance_id,id)`. The table is created inside the existing destination import transaction **after** the matched backup; prevalidation accepts a valid older store without the table read-only. Existing exact records are accepted without rewriting and conflicting records fail. Admission `identity_projections` and source core rows remain unchanged. Reports contain only counts and row/key digests, not raw IDs or values.

Focused validation:

- `cargo test -p opencrab-gateway-migrate --test s8` — 7/7, including original-ID/metadata assertion and import rerun.
- `cargo test -p opencrab-gateway-migrate --test s8_web` — 5/5, including multiple inert source roles/IDs and a pre-existing Web store without the new table.
- `cargo test -p opencrab-gateway-migrate --lib` — 16/16, including Nostr old-store read-only validation and per-instance same-source-ID acceptance/conflict.
- `cargo fmt --all -- --check`, `bash scripts/check-file-size.sh`, `git diff --check` — pass.

This is disposable-database evidence only. It does **not** approve S10 deletion, real-data migration, QC deployment, or a complete matched rollback rehearsal. S10 must independently verify every exact source edge before deleting any original identity row.
