# Issue #1006 S8 — focused final-review RED

Base: `a8a8cb69db8513aad4e9a458a0086c5e5120cb55`. Tests only; no migrator production implementation changed.

| Normal-use seam | Focused test command (`cargo test -p opencrab-gateway-migrate --test s8 <name> -- --exact --nocapture`) | Assertion-level RED |
|---|---|---|
| Completed `project-core-state` rerun with its original verification file intact | `s8_import_and_project_are_offline_idempotent_and_preserve_core_rows` | `completed project rerun must accept its existing verification: existing verification differs` (`tests/s8.rs:277`). The original file is no longer deleted by the fixture; subsequent assertions require unchanged bytes and the original returned evidence. |
| Retained Discord config + secret for an agent whose only active instance is Nostr | `s8_refuses_retained_discord_config_with_only_nostr_instance_before_backup` | `retained Discord credential/config must not be silently matched to a Nostr plan` (`tests/s8.rs`). The valid stopped migration instead returned successful import while ignoring the retained Discord source. The assertion also requires refusal before the backup directory exists. |

Both tests compiled and reached their intended behavioral assertions; neither failure is a fixture failure. `cargo fmt --all -- --check` and `git diff --check` pass.

Per owner direction, no pre-backup existing-destination conflict assertion was added: that timing hardening is non-gating Issue #1016. Unsorted-approval normalization was not trivial within either focused fixture and has not been proven RED here; it remains a separate contract discrepancy, not a GREEN claim. No S9+, README, production, real-data, or stash changes.
