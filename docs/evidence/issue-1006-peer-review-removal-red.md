# Issue #1006 peer-review removal RED

At `806c598`, before any production change:

- `cargo test -q -p opencrab-server --lib removed_peer_review_is_absent_from_production_action_catalog -- --nocapture` failed: the production-owned catalog still exposed `request_peer_review`.
- `cargo test -q -p opencrab-server --lib inbound_speech_does_not_create_peer_review_progress_with_or_without_legacy_identity_table -- --nocapture` failed at the first legitimate registered co-agent inbound: task progress had 2 rows instead of the retained original 1. The same fixture also asserts speech persistence and behavior after dropping only the disposable legacy identity table, once this first assertion passes.

Both were assertion failures (exit 101), not compilation errors. No real DB was changed.

## GREEN

The production action definition, execution branch, inbound verdict subscriber, dead roster lookup, feature-only transport surface, and orphaned feature tests were removed. The generic task/progress/history tables, co-agent relationships, and historical migration SQL were not changed. The generic inbound observation hook remains for existing conformance runtimes but has no production subscriber.

Focused results: `opencrab-server --lib` 452/452, `opencrab-actions --lib` 364/364, `opencrab-db --lib` 267 passed (3 ignored), `opencrab-core --lib` 474/474, `opencrab-gateway --lib` 3/3, and the `opencrab-extgate --test conformance author_label_is_persisted_and_reaches_live_turn` fixture 1/1. `cargo fmt --check`, `git diff --check`, and `scripts/check-file-size.sh` passed. No live environment or real DB was touched.
