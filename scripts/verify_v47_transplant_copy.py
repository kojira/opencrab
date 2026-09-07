#!/usr/bin/env python3
"""Data/schema invariants for the read-only v43 -> v47 copy rehearsal."""

import hashlib
import json
import sqlite3
import sys
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
        for name, value in zip(selected, row):
            digest.update(name.encode("utf-8"))
            digest.update(b"\0")
            if value is None:
                cell = b"NULL"
            elif isinstance(value, bytes):
                cell = b"B:" + value.hex().encode("ascii")
            elif isinstance(value, int):
                cell = f"I:{value}".encode("ascii")
            elif isinstance(value, float):
                cell = f"R:{value}".encode("ascii")
            else:
                cell = f"T:{value}".encode("utf-8")
            digest.update(cell)
            digest.update(b"\1")
        digest.update(b"\2")
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


def capture_before(pristine: Path, output: Path) -> None:
    conn = connect_ro(pristine)
    try:
        version = conn.execute("PRAGMA user_version").fetchone()[0]
        if version != SOURCE_VERSION:
            raise SystemExit(f"source copy user_version={version}; want {SOURCE_VERSION}")
        errors = integrity_errors(conn)
        if errors:
            raise SystemExit("source copy invalid: " + "; ".join(errors))
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
        "SELECT COUNT(*) FROM agents WHERE subject_id IS NULL OR subject_id <= 0"
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


def verify_after(before_path: Path, copy_a: Path, copy_b: Path) -> None:
    before = json.loads(before_path.read_text(encoding="utf-8"))
    a = connect_ro(copy_a)
    b = connect_ro(copy_b)
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
    finally:
        a.close()
        b.close()
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


def main() -> None:
    if len(sys.argv) < 2:
        raise SystemExit("usage: verify_v47_transplant_copy.py before|after ...")
    if sys.argv[1] == "before" and len(sys.argv) == 4:
        capture_before(Path(sys.argv[2]), Path(sys.argv[3]))
    elif sys.argv[1] == "after" and len(sys.argv) == 5:
        verify_after(Path(sys.argv[2]), Path(sys.argv[3]), Path(sys.argv[4]))
    else:
        raise SystemExit("invalid arguments")


if __name__ == "__main__":
    main()
