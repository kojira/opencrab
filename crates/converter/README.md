# opencrab-converter operational contract

`migrate_in_place(conn, config, environment, captured_at)` applies store schema and fills
new tables from old ones on the same SQLite connection. The explicit config file,
environment file, and `--captured-at` UTC nanoseconds are the input snapshot authority.

Operator path for a non-empty legacy database:

1. Stop every process that can write the database.
2. Run `opencrab-converter --db <opencrab.db> --config <default.toml> --environment <effective.env> --captured-at <utc-nanos>`.
3. A present `schema_migration_state.inplace-v1` marker fails loud. Re-run only on a pristine copy.

App startup (`ensure_migrated`) is a no-op when the marker is present. A non-empty legacy
database without the marker fails loud and does not serve. A fresh database (legacy tables
empty or absent) runs `migrate_in_place` in place.
