# Issue #1006 S8 Web: forward assertion-level RED

Base HEAD: `96dd1c89b9e299f33273686b297774d1f979c60a` (clean before the test edit). This checkpoint edits only the integration test and this evidence; no production source, live database, deployer, or stash changes.

`crates/gateway-migrate/tests/s8_web.rs::s8_web_existing_bearer_preserves_independent_source_identity_and_installs_credential` builds a disposable schema-56 core with a Web source identity whose `user_id` differs from configured/persisted `author_id`, and a real S5 WebStore instance with the same author, one matching `trusted_user` bearer role, and a valid NULL credential envelope. It approves the single identity edge, supplies a mode-0600 operator credential and S5 master key, and invokes the real `command::run_import`/`command::run_project` path. After successful import, assertions require a separate preserved non-bearer identity projection, unchanged bearer/author, encrypted credential decryptable with S5 Web key, and unchanged credential envelope on a successful rerun.

Exact command after formatting:

```text
cargo test -p opencrab-gateway-migrate --test s8_web -- --exact s8_web_existing_bearer_preserves_independent_source_identity_and_installs_credential
```

Exit code `101` (the Rust test failed, not compilation/fixture setup). Relevant output:

```text
running 1 test
test s8_web_existing_bearer_preserves_independent_source_identity_and_installs_credential ... FAILED
thread 's8_web_existing_bearer_preserves_independent_source_identity_and_installs_credential' panicked at crates/gateway-migrate/tests/s8_web.rs:112:53:
existing Web instance and bearer must accept approved source identity and credential: approved destination row conflicts with before-backup
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
```

The failure is inside destination verification of an existing S5 Web instance: the current generic instance expectation does not match S5 Web's author/bearer semantic row. It is not yet evidence that the separate identity projection or NULL-credential install has failed at its own seam: those assertions cannot execute until the existing Web instance is accepted. No unsupported-role/cardinality refusal is claimed RED here for the same reason. Follow the approved D4/D5 contract at each next seam without inventing a non-S5 fixture.

`cargo fmt --all -- --check`: exit 0.
