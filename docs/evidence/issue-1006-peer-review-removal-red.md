# Issue #1006 peer-review removal RED

At `806c598`, before any production change:

- `cargo test -q -p opencrab-server --lib removed_peer_review_is_absent_from_production_action_catalog -- --nocapture` failed: the production-owned catalog still exposed `request_peer_review`.
- `cargo test -q -p opencrab-server --lib inbound_speech_does_not_create_peer_review_progress_with_or_without_legacy_identity_table -- --nocapture` failed at the first legitimate registered co-agent inbound: task progress had 2 rows instead of the retained original 1. The same fixture also asserts speech persistence and behavior after dropping only the disposable legacy identity table, once this first assertion passes.

Both were assertion failures (exit 101), not compilation errors. No real DB was changed.
