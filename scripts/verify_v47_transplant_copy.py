#!/usr/bin/env python3
"""Data/schema invariants for the read-only v43 -> v47 copy rehearsal."""

import hashlib
import json
import sqlite3
import sys
import tempfile
from pathlib import Path

SOURCE_VERSION = 43
TARGET_VERSION = 47
NEW_TABLES = {
    "conversation_snapshots",
    "deliveries",
    "external_origins",
    "gate_bindings",
    "gate_instances",
    "gateway_operation_calls",
    "nostr_bundle_state",
}
ALLOWED_ADDED_COLUMNS = {"agents": {"subject_id"}}


def connect_ro(path: Path) -> sqlite3.Connection:
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True)


def connect_work_copy(path: Path) -> sqlite3.Connection:
    # initialize() configures WAL. The rehearsal owns these temporary copies,
    # so allow SQLite to open/checkpoint their WAL state normally.
    return sqlite3.connect(path)


def user_tables(conn: sqlite3.Connection) -> list[str]:
    return [
        row[0]
        for row in conn.execute(
            "SELECT name FROM sqlite_master "
            "WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
        )
    ]


def columns(conn: sqlite3.Connection, table: str) -> list[str]:
    return [row[1] for row in conn.execute(f'PRAGMA table_info("{table}")')]


def hash_frame(digest: object, tag: bytes, payload: bytes) -> None:
    digest.update(tag)
    digest.update(len(payload).to_bytes(8, "big"))
    digest.update(payload)


def table_digest(
    conn: sqlite3.Connection, table: str, selected: list[str]
) -> dict[str, object]:
    info = list(conn.execute(f'PRAGMA table_info("{table}")'))
    primary_key = sorted(
        ((row[5], row[1]) for row in info if row[5] > 0), key=lambda item: item[0]
    )
    order = ", ".join(f'"{name}"' for _, name in primary_key) or "rowid"
    quoted = ", ".join(f'"{name}"' for name in selected)
    count = conn.execute(f'SELECT COUNT(*) FROM "{table}"').fetchone()[0]
    digest = hashlib.sha256()
    for row in conn.execute(f'SELECT {quoted} FROM "{table}" ORDER BY {order}'):
        digest.update(b"R")
        for name, value in zip(selected, row):
            hash_frame(digest, b"N", name.encode("utf-8"))
            if value is None:
                cell = b""
                tag = b"0"
            elif isinstance(value, bytes):
                cell = value
                tag = b"B"
            elif isinstance(value, int):
                cell = str(value).encode("ascii")
                tag = b"I"
            elif isinstance(value, float):
                cell = repr(value).encode("ascii")
                tag = b"F"
            else:
                cell = value.encode("utf-8")
                tag = b"T"
            hash_frame(digest, tag, cell)
    return {"count": count, "digest": digest.hexdigest(), "columns": selected}


def schema_snapshot(conn: sqlite3.Connection) -> list[tuple[str, str, str]]:
    return [
        (row[0], row[1], row[2] or "")
        for row in conn.execute(
            "SELECT type, name, sql FROM sqlite_master "
            "WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name"
        )
    ]


def full_snapshot(conn: sqlite3.Connection) -> dict[str, object]:
    return {
        "user_version": conn.execute("PRAGMA user_version").fetchone()[0],
        "schema": schema_snapshot(conn),
        "tables": {
            table: table_digest(conn, table, columns(conn, table))
            for table in user_tables(conn)
        },
    }


def integrity_errors(conn: sqlite3.Connection) -> list[str]:
    errors = []
    integrity = [row[0] for row in conn.execute("PRAGMA integrity_check")]
    if integrity != ["ok"]:
        errors.append(f"integrity_check={integrity!r}")
    foreign_keys = list(conn.execute("PRAGMA foreign_key_check"))
    if foreign_keys:
        errors.append(f"foreign_key_check rows={len(foreign_keys)}")
    return errors


def bundle_manifest(path: Path, output: Path) -> None:
    manifest = {}
    for suffix in ("", "-wal", "-shm"):
        item = Path(str(path) + suffix)
        if not item.exists():
            manifest[suffix] = {"exists": False}
            continue
        digest = hashlib.sha256()
        with item.open("rb") as handle:
            while chunk := handle.read(1024 * 1024):
                digest.update(chunk)
        manifest[suffix] = {
            "exists": True,
            "size": item.stat().st_size,
            "sha256": digest.hexdigest(),
        }
    output.write_text(json.dumps(manifest, sort_keys=True), encoding="utf-8")


def capture_before(pristine: Path, output: Path) -> None:
    conn = connect_ro(pristine)
    try:
        version = conn.execute("PRAGMA user_version").fetchone()[0]
        if version != SOURCE_VERSION:
            raise SystemExit(f"source copy user_version={version}; want {SOURCE_VERSION}")
        errors = integrity_errors(conn)
        if errors:
            raise SystemExit("source copy invalid: " + "; ".join(errors))
        unexpected = set(user_tables(conn)) & NEW_TABLES
        if unexpected:
            raise SystemExit(
                "v43 source contains future tables: " + ", ".join(sorted(unexpected))
            )
        snapshot = full_snapshot(conn)
    finally:
        conn.close()
    output.write_text(json.dumps(snapshot, sort_keys=True), encoding="utf-8")
    print(f"before tables={len(snapshot['tables'])} user_version={version}")


def compare_existing(
    before: dict[str, object], after_conn: sqlite3.Connection
) -> list[str]:
    errors = []
    after_tables = set(user_tables(after_conn))
    before_tables = before["tables"]
    for table, expected in before_tables.items():
        if table not in after_tables:
            errors.append(f"existing table missing: {table}")
            continue
        before_columns = expected["columns"]
        actual_columns = columns(after_conn, table)
        added = set(actual_columns) - set(before_columns)
        allowed = ALLOWED_ADDED_COLUMNS.get(table, set())
        if added != allowed:
            errors.append(
                f"existing table columns changed: {table} added={sorted(added)} "
                f"want={sorted(allowed)}"
            )
        missing = set(before_columns) - set(actual_columns)
        if missing:
            errors.append(f"existing table columns missing: {table} {sorted(missing)}")
            continue
        actual = table_digest(after_conn, table, before_columns)
        if actual != expected:
            errors.append(
                f"existing table data changed: {table} "
                f"before_count={expected['count']} after_count={actual['count']}"
            )
    expected_tables = set(before_tables) | NEW_TABLES
    if after_tables != expected_tables:
        errors.append(
            "user tables outside closed set: "
            f"added={sorted(after_tables - expected_tables)} "
            f"missing={sorted(expected_tables - after_tables)}"
        )
    return errors


def verify_subject_ids(conn: sqlite3.Connection) -> list[str]:
    null_or_bad = conn.execute(
        "SELECT COUNT(*) FROM agents "
        "WHERE subject_id IS NULL OR typeof(subject_id) != 'integer' OR subject_id <= 0"
    ).fetchone()[0]
    duplicates = conn.execute(
        "SELECT COUNT(*) FROM ("
        "SELECT subject_id FROM agents GROUP BY subject_id HAVING COUNT(*) > 1)"
    ).fetchone()[0]
    errors = []
    if null_or_bad:
        errors.append(f"agents.subject_id null/non-positive rows={null_or_bad}")
    if duplicates:
        errors.append(f"agents.subject_id duplicate groups={duplicates}")
    return errors


def normalized_sql(sql: str | None) -> str:
    if sql is None:
        return ""
    return " ".join(sql.replace("IF NOT EXISTS", "").split())


def index_catalog(conn: sqlite3.Connection, table: str) -> list[object]:
    indexes = []
    for row in conn.execute(f'PRAGMA index_list("{table}")'):
        name, unique, origin, partial = row[1], row[2], row[3], row[4]
        index_columns = list(conn.execute(f'PRAGMA index_info("{name}")'))
        sql_row = conn.execute(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name=?", (name,)
        ).fetchone()
        indexes.append(
            (name, unique, origin, partial, index_columns, normalized_sql(sql_row[0] if sql_row else None))
        )
    return sorted(indexes, key=lambda item: item[0])


def migration_catalog(conn: sqlite3.Connection) -> dict[str, object]:
    table_details = {}
    for table in sorted(NEW_TABLES):
        table_details[table] = {
            "columns": list(conn.execute(f'PRAGMA table_info("{table}")')),
            "foreign_keys": list(conn.execute(f'PRAGMA foreign_key_list("{table}")')),
            "indexes": index_catalog(conn, table),
        }
    object_names = {
        "idx_agents_subject_id",
        "agents_subject_id_insert_guard",
        "agents_subject_id_assign",
        "agents_subject_id_update_guard",
    }
    objects = [
        (kind, name, table, normalized_sql(sql))
        for kind, name, table, sql in conn.execute(
            "SELECT type, name, tbl_name, sql FROM sqlite_master "
            "WHERE type IN ('index', 'trigger') AND "
            "(tbl_name IN (%s) OR name IN (%s)) ORDER BY type, name"
            % (
                ",".join("?" for _ in NEW_TABLES),
                ",".join("?" for _ in object_names),
            ),
            tuple(sorted(NEW_TABLES)) + tuple(sorted(object_names)),
        )
    ]
    subject = [
        row
        for row in conn.execute('PRAGMA table_info("agents")')
        if row[1] == "subject_id"
    ]
    return {"tables": table_details, "objects": objects, "subject_id": subject}


def verify_after(
    before_path: Path, copy_a: Path, copy_b: Path, fresh_path: Path
) -> None:
    before = json.loads(before_path.read_text(encoding="utf-8"))
    a = connect_work_copy(copy_a)
    b = connect_work_copy(copy_b)
    fresh = connect_work_copy(fresh_path)
    errors = []
    try:
        for label, conn in (("A", a), ("B", b)):
            version = conn.execute("PRAGMA user_version").fetchone()[0]
            if version != TARGET_VERSION:
                errors.append(f"copy {label} user_version={version}; want {TARGET_VERSION}")
            errors.extend(f"copy {label}: {item}" for item in integrity_errors(conn))
            errors.extend(f"copy {label}: {item}" for item in compare_existing(before, conn))
            errors.extend(f"copy {label}: {item}" for item in verify_subject_ids(conn))
            for table in sorted(NEW_TABLES):
                if table in user_tables(conn):
                    count = conn.execute(f'SELECT COUNT(*) FROM "{table}"').fetchone()[0]
                    if count:
                        errors.append(f"copy {label}: new table {table} count={count}; want 0")
        if full_snapshot(a) != full_snapshot(b):
            errors.append("copy A and twice-initialized copy B differ")
        expected_catalog = migration_catalog(fresh)
        if migration_catalog(a) != expected_catalog:
            errors.append("copy A migration schema differs from fresh v47 catalog")
        if migration_catalog(b) != expected_catalog:
            errors.append("copy B migration schema differs from fresh v47 catalog")
    finally:
        a.close()
        b.close()
        fresh.close()
    if errors:
        print("v47 copy verification RED:")
        for error in errors:
            print(f"  - {error}")
        raise SystemExit(1)
    print("v47 copy verification GREEN")
    print("  existing table counts/digests unchanged")
    print("  schema additions match the closed v44-v47 set")
    print("  new tables empty; agents.subject_id valid")
    print("  integrity/foreign keys OK; two-copy/no-op result matched")


def self_test() -> None:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        malformed = root / "malformed.db"
        conn = sqlite3.connect(malformed)
        conn.executescript(
            "CREATE TABLE gateway_operation_calls (wrong TEXT); PRAGMA user_version=43;"
        )
        conn.close()
        try:
            capture_before(malformed, root / "before.json")
        except SystemExit as error:
            if "future tables" not in str(error):
                raise
        else:
            raise AssertionError("future table in v43 source was accepted")

        first = sqlite3.connect(root / "first.db")
        second = sqlite3.connect(root / "second.db")
        first.execute("CREATE TABLE t (a TEXT, b TEXT)")
        second.execute("CREATE TABLE t (a TEXT, b TEXT)")
        first.execute("INSERT INTO t VALUES (?, ?)", ("x\u0001N", "y"))
        second.execute("INSERT INTO t VALUES (?, ?)", ("x", "N\u0001y"))
        assert table_digest(first, "t", ["a", "b"]) != table_digest(
            second, "t", ["a", "b"]
        )
        first.close()
        second.close()

        typed = sqlite3.connect(root / "typed.db")
        typed.execute("CREATE TABLE agents (subject_id)")
        typed.execute("INSERT INTO agents VALUES (1.5)")
        assert verify_subject_ids(typed)
        typed.close()

        bundle = root / "bundle.db"
        bundle.write_bytes(b"db")
        Path(str(bundle) + "-wal").write_bytes(b"uncheckpointed-wal")
        manifest_before = root / "bundle-before.json"
        bundle_manifest(bundle, manifest_before)
        state = json.loads(manifest_before.read_text(encoding="utf-8"))
        assert state["-wal"]["exists"] is True
        assert state["-shm"]["exists"] is False
        Path(str(bundle) + "-shm").write_bytes(b"new-shm")
        manifest_after = root / "bundle-after.json"
        bundle_manifest(bundle, manifest_after)
        assert manifest_before.read_bytes() != manifest_after.read_bytes()
    print("verify_v47_transplant_copy self-test GREEN")


def main() -> None:
    if len(sys.argv) < 2:
        raise SystemExit(
            "usage: verify_v47_transplant_copy.py manifest|before|after|self-test ..."
        )
    if sys.argv[1] == "manifest" and len(sys.argv) == 4:
        bundle_manifest(Path(sys.argv[2]), Path(sys.argv[3]))
    elif sys.argv[1] == "before" and len(sys.argv) == 4:
        capture_before(Path(sys.argv[2]), Path(sys.argv[3]))
    elif sys.argv[1] == "after" and len(sys.argv) == 6:
        verify_after(
            Path(sys.argv[2]), Path(sys.argv[3]), Path(sys.argv[4]), Path(sys.argv[5])
        )
    elif sys.argv[1] == "self-test" and len(sys.argv) == 2:
        self_test()
    else:
        raise SystemExit("invalid arguments")


if __name__ == "__main__":
    main()
