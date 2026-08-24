# opencrab-converter operational contract

`migrate_in_place(conn, config, environment, captured_at)` applies store schema and fills
new tables from old ones on the same SQLite connection. The explicit config file,
environment file, and `--captured-at` UTC nanoseconds are the input snapshot authority.

Operator path for a non-empty legacy database:

1. Stop every process that can write the database.
2. Run `opencrab-converter --db <opencrab.db> --config <default.toml> --environment <effective.env> --captured-at <utc-nanos>`.
3. A present `schema_migration_state.inplace-v1` marker fails loud. Re-run only on a pristine copy.

App startup (`ensure_migrated`) is decoupled from conversion. It looks only at
body-legacy sentinel tables (`agents`, `sessions` — names the old
implementation had that store SCHEMA does not; `skills` is now a store table),
by existence, not row counts:

- sentinel present and no `inplace-v1` marker → fail loud
  (`this is a legacy-implementation DB; run the migration`) and do not serve
- sentinel present and marker present → boot (migrated body DB)
- no sentinel → boot. No marker is required; converter is not invoked;
  `migrate_in_place` does not run on the startup path

The marker is a record written by this command. A second run still fails loud
when `schema_migration_state.inplace-v1` is already present.

The environment snapshot must contain resolved assignment values. A `$` on a non-comment,
non-blank line fails loud when `migrate_in_place` loads that snapshot (the CLI already
has the database connection open). Comment lines (optional leading whitespace then `#`)
and blank lines are not inspected. Every other line is scanned in full if it has no `=`,
or after the first `=` if it has one.
