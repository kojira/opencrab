# opencrab-converter operational contract

The converter accepts an immutable SQLite snapshot made from exactly one checkpointed main
database file. The main database bytes, the explicit config and environment snapshots, and
`--captured-at` form the input snapshot authority. SQLite WAL bytes are not part of that authority.

Prepare a source snapshot as follows:

1. Stop every process that can write the source database.
2. Using a writable SQLite connection, run `PRAGMA wal_checkpoint(TRUNCATE);` and verify it
   completes successfully. If the database uses WAL mode, switch the quiesced database out of WAL
   mode when practical.
3. Close the SQLite connection. Verify that `<source>-wal`, `<source>-shm`, and
   `<source>-journal` are absent or empty. A rollback journal may represent a hot journal and must
   not be recovered by the converter.
4. Copy the checkpointed main database file by itself to an immutable staging location. Do not
   start or retain a writer against that staged file. Keep its bytes stable until `convert()` (or
   the converter process) has returned; opening an immutable SQLite connection does not pin every
   unread page at open time.
5. Pass the staged main file to `--source`, together with immutable config/environment snapshots
   and their explicit UTC-nanosecond capture time in `--captured-at`.

The converter fails loudly before reading source rows when any non-empty `-wal`, `-shm`, or
`-journal` sibling is present. It URI-encodes the source path and opens it as
`file:...?mode=ro&immutable=1` with SQLite URI handling enabled. It does not merge sidecar bytes
into the digest and does not run checkpoint or journal recovery on the operator's behalf.
`immutable=1` makes SQLite trust the caller's immutability guarantee; it is not a snapshot-copying
mechanism. The converter checks the main-file digest again before committing the target and fails
loudly if it observes that the staged main bytes changed during conversion.
