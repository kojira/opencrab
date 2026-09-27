# Issue #1006 S10 static DB boundary — forward RED

Checkpoint base: `db10064` (owner-approved peer-review removal). This is test-only evidence; production schema/query/runtime sources were not edited.

- `cargo test -p opencrab-db --lib s10_fresh_schema_omits_legacy_gateway_tables_but_retains_generic_state -- --nocapture` **fails at the new assertion**: `fresh core schema must not create legacy gateway table trusted_users`. The fixture calls the production `opencrab_db::init_memory()` initializer; it also requires retained `api_principals`, agents, sessions, gate instances, and deliveries. Exit 101.
- `cargo test -p opencrab-gateway-migrate --test s8 s10_cleanup_after_freeze_preserves_retained_core_state -- --nocapture` **passes**. This test performs the actual guarded offline cleanup on disposable core/gateway DBs, reopens core via production `opencrab_db::init_connection`, asserts no legacy tables were recreated and resolves the retained REST `api_principals` row. The current initializer skips fresh-schema SQL when the cleaned database is already at v56. This is not RED evidence and must not be represented as such.
- `cargo fmt --all -- --check` and `git diff --check` pass.

Next minimal GREEN: omit the five legacy concrete tables from freshly initialized core without altering historical migration SQL; remove the now-unused production concrete query/re-export seams while preserving generic co-agent relationships and REST principal lookup. No real database cleanup or deployment is authorized by this test.
