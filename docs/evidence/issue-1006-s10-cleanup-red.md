# Issue #1006 S10 guarded cleanup — forward RED

Baseline: `d2cf11908fb2d14853a0b622f6ed3eaaf6db8d2e`. No production source or real database was changed.

One disposable schema-56 core plus one Discord S5 store follows successful S8 import/project. A normal `last_fired_at` advancement precedes the complete core-plus-gateway post-QC freeze; `verify-freeze` succeeds. The gateway's inert source record is checked against all eight original `trusted_users` fields. The new integration assertion invokes `clean-legacy-state` with the same approved manifest and exact participating destination, then requires removal of legacy concrete tables, a distinct applied record bound to the frozen set, and preservation of subjects, sessions, binding, tool history, deliveries, heartbeat progress, REST principal, and immutable projection marker.

Command: `cargo test -p opencrab-gateway-migrate --test s8 s10_cleanup_after_freeze_preserves_retained_core_state -- --exact`

Observed authentic RED: test fails at its first cleanup-success assertion because the executable returns `opencrab-gateway-migrate: unknown command`. The post-command deletion, retained-state, and applied-record assertions are **not yet reached** and are not GREEN or independent RED evidence.

The separate post-preflight mutation fixture (one of the gateway identity source record's eight fields changed before cleanup) is **not added yet**: it would only fail for the same missing CLI command, not establish that the cleanup guard refuses mutation before core deletion. Add that focused assertion after the command exists and before the mutation guard's production implementation. No locks/race framework or #1016 hardening is introduced here.

Checks: `cargo fmt -p opencrab-gateway-migrate --check`, `scripts/check-file-size.sh`, and `git diff --check` pass. No QC/production process or database was accessed.
