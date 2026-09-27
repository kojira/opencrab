# Issue #1006 S8 Web: existing instance semantic checkpoint

Base RED: `681427cbd58095d64d5c9ca3d349f23485632133` and `issue-1006-s8-web-red.md`. This checkpoint changes only Web **instance semantic verification** (plus its existing unit assertion); it does not import an identity, install a credential, or prove a completed Web migration.

The approved Web core config's `author_id` is decoded and compared with the already persisted S5 Web `instances.author_id`, alongside instance ID, agent, revision, and enabled status. Existing bearer role cardinality still must equal one; the role, policies, credential, and instance are not modified by this verification. Discord/Nostr instance semantics are unchanged.

The identical forward integration assertion now passes its previous `approved destination row conflicts with before-backup` seam and fails later with exit 101:

```text
cargo test -p opencrab-gateway-migrate --test s8_web -- --exact s8_web_existing_bearer_preserves_independent_source_identity_and_installs_credential
thread ... panicked at crates/gateway-migrate/tests/s8_web.rs:112:53:
existing Web instance and bearer must accept approved source identity and credential: Invalid column type Null at index: 0, name: credential_envelope
```

The new failure occurs in `destination_plans.rs::select_credential`: the valid S5 Web instance has a NULL envelope and the selection query reads it as `String` before `optional().flatten()` can handle NULL. This is forward assertion-level RED for Web credential selection, **not GREEN** for credential installation or external identity disposition. Stop here; the next production edit requires its own scoped fix.

`cargo test -p opencrab-gateway-migrate --lib destination::s8_review_red_tests::existing_web_instance_uses_persisted_author_and_role_without_creation` passed (1/1). `cargo fmt --all -- --check`, `bash scripts/check-file-size.sh` (all Rust files <=800 lines), and `git diff --check` passed.
