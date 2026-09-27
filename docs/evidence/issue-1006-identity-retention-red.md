# D-1006-ID-01 assertion-level RED (docs and test only)

Baseline: `6428314c6687c4e300973c0047932d6434507e84`. Source: `crates/gateway-migrate/tests/s8.rs` existing populated Discord migration fixture; its approved `trusted_users` row contains original `id`, `user_id`, `agent_id`, exact `permission`, `created_by`, `created_at`, `display_name`, and `platform`. The source remains in core after S8, but the Discord destination currently records only normalized `identity_projections` values. S10 deletion would otherwise discard the original ID and metadata.

RED command (after targeted rustfmt):

```text
cargo test -p opencrab-gateway-migrate --test s8 s8_import_and_project_are_offline_idempotent_and_preserve_core_rows -- --exact --nocapture
```

Actual: one test failed at `crates/gateway-migrate/tests/s8.rs:219`, assertion `S8 must retain the gateway identity's original ID and metadata before S10 may delete its source`. This is the required destination absence, not a compile error or unrelated migration failure. The adjacent value assertion is intentionally unreachable until the required table exists; it then checks all eight original fields verbatim. `cargo fmt --all -- --check` passed after formatting. No production source, real database, or S10 cleanup was modified. **RED is not GREEN.**
