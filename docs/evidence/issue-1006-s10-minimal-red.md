# Issue #1006 S10 minimal disposable RED

Base: `63d5e4fa6b9a2f89c677be670a615b4acfa11526`. No S10 production source, real DB, QC runtime, or production data was changed.

`crates/gateway-migrate/tests/s8.rs::s10_verify_freeze_is_read_only_for_projected_discord_fixture` reuses the passing S8 projected v56 core/Discord fixture. The test first completes S8 import, original eight-field identity retention and `project-core-state`, then creates a new matched SQLite-consistent post-QC core-plus-participating-Discord snapshot. Its external freeze fixture binds the approval, projection verification, immutable projection marker, approved dispositions and complete snapshot inventory. It invokes the actual `opencrab-gateway-migrate verify-freeze` binary and asserts success followed by byte and logical digest equality of both live databases.

Assertion-level RED:

```text
cargo test -p opencrab-gateway-migrate --test s8 s10_verify_freeze_is_read_only_for_projected_discord_fixture -- --exact --nocapture
S10 verify-freeze must accept a matched projected fixture without writing: opencrab-gateway-migrate: unknown command
FAILED (assertion in s8.rs)
```

Existing S8 companion fixture stays GREEN:

```text
cargo test -p opencrab-gateway-migrate --test s8 s8_import_and_project_are_offline_idempotent_and_preserve_core_rows -- --exact
ok (1 passed)
```

This is **one representative S10 RED**, not freeze/cleanup GREEN. The missing CLI operation is observed only after a valid projected fixture and matched freeze exist. Still untested: all-participating multi-gateway inventory, missing/extra store rejection, post-freeze mutation refusal before deletion, eight-field edge proof at cleanup, atomic deletion plus separate applied record, retained-state/marker immutability after cleanup, locks, rollback/fault injection. The exact freeze JSON shape here is a test fixture for the intended external evidence, not a claim that an implementation contract was already approved. No real-data cleanup is authorized.
