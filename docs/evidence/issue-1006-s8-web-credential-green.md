# Issue #1006 S8 Web: existing nullable credential

Base: `6180465df6ad78cfce6eb4304744b99beb76a6d5`. Its integration test first failed at Web credential selection with `Invalid column type Null ... credential_envelope` (`issue-1006-s8-web-instance-green.md`).

After reading the existing S5 Web envelope as nullable in `select_credential`, the same exact test again failed with exit 101 and `Invalid column type Null ... credential_envelope`, now at the existing-row read in `apply_instance`. This second assertion-level RED was captured before editing `apply_instance`; it was **not** a Web GREEN.

`apply_instance` now encrypts and installs the approved operator credential only when an existing Web instance has a NULL envelope. An existing non-NULL envelope is decrypted and compared without rewriting; non-NULL Discord/Nostr semantics remain unchanged. The existing instance author, bearer role, policies, and updated timestamp are untouched.

The unchanged exact integration assertion now passes (1/1):

```text
cargo test -p opencrab-gateway-migrate --test s8_web -- --exact s8_web_existing_bearer_preserves_independent_source_identity_and_installs_credential
```

This test verifies the imported source-user projection is distinct from the admitted bearer, Web author/role are retained, the installed credential decrypts, core projection succeeds, and a completed rerun retains the same envelope. `cargo fmt --all -- --check`, `bash scripts/check-file-size.sh`, and `git diff --check` passed. This is focused Web GREEN, not S8 review approval or release authorization.
