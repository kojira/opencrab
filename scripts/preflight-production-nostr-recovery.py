#!/usr/bin/env python3
"""Clone-only database invariants for D-RB-001.

This module is used both by the packaged read-only preflight and by the authentic
clone-topology validator.  It never accepts a database outside ``--clone-root``.
"""
from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
import sqlite3
from pathlib import Path
from typing import Any

# Every D-RB-001 data class, including all production memory tables and model
# settings.  Missing tables are represented explicitly so an old clone cannot
# silently masquerade as a schema-52 clone.
TABLES = (
    "agents",
    "sessions",
    "memory_curated",
    "memory_sessions",
    "memory_sessions_fts",
    "conversation_snapshots",
    "memory_index_nodes",
    "memory_category_members",
    "memory_index_watermark",
    "agent_memory_index_config",
    "tool_logs",
    "llm_logs",
    "agent_nostr_config",
    "trusted_users",
    "trusted_co_agents",
    "gate_instances",
    "gate_bindings",
    "llm_provider_overrides",
    "model_pricing",
)


def contained(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False


def digest_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _canonical(value: Any) -> Any:
    if isinstance(value, bytes):
        return {"blob_sha256": hashlib.sha256(value).hexdigest(), "bytes": len(value)}
    return value


def _canonical_rows(connection: sqlite3.Connection, table: str) -> list[str]:
    columns = [
        str(row[1])
        for row in connection.execute(f'PRAGMA table_info("{table}")').fetchall()
    ]
    if not columns:
        return []
    quoted = ", ".join('"' + column.replace('"', '""') + '"' for column in columns)
    rows = []
    for row in connection.execute(f'SELECT {quoted} FROM "{table}"'):
        rows.append(
            json.dumps(
                [_canonical(value) for value in row],
                ensure_ascii=False,
                separators=(",", ":"),
                sort_keys=True,
            )
        )
    rows.sort()
    return rows


def digest_table(connection: sqlite3.Connection, table: str) -> dict[str, object]:
    exists = connection.execute(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name=?", (table,)
    ).fetchone()
    if not exists:
        return {"exists": False, "rows": 0, "sha256": None, "canonical_rows": []}
    rows = _canonical_rows(connection, table)
    digest = hashlib.sha256()
    for row in rows:
        digest.update(row.encode())
        digest.update(b"\n")
    return {
        "exists": True,
        "rows": len(rows),
        "sha256": digest.hexdigest(),
        # Retained only in mode-0600 validation artifacts.  Keeping canonical
        # rows allows an assertion-level no-rewrite/no-deletion comparison while
        # still permitting normal append-only startup activity.
        "canonical_rows": rows,
    }


def integrity_checks(connection: sqlite3.Connection) -> dict[str, list[str]]:
    result: dict[str, list[str]] = {}
    for pragma in ("quick_check", "integrity_check"):
        rows = [str(row[0]) for row in connection.execute(f"PRAGMA {pragma}").fetchall()]
        result[pragma] = rows
        if rows != ["ok"]:
            raise ValueError(f"PRAGMA {pragma} failed: {rows}")
    return result


def snapshot_database(database: Path) -> dict[str, object]:
    connection = sqlite3.connect(f"file:{database}?mode=ro", uri=True)
    try:
        checks = integrity_checks(connection)
        schema = int(connection.execute("PRAGMA user_version").fetchone()[0])
        enabled = int(
            connection.execute(
                "SELECT count(*) FROM agent_nostr_config WHERE enabled=1"
            ).fetchone()[0]
        )
        owners = int(
            connection.execute(
                "SELECT count(*) FROM agent_nostr_config "
                "WHERE enabled=1 AND trim(owner_pubkey)<>''"
            ).fetchone()[0]
        )
        tables = {table: digest_table(connection, table) for table in TABLES}
    finally:
        connection.close()
    return {
        "schema": schema,
        "checks": checks,
        "enabled_nostr_agents": enabled,
        "enabled_nostr_owners": owners,
        "tables": tables,
    }


def compare_snapshots(before: dict[str, object], after: dict[str, object]) -> dict[str, object]:
    violations: list[dict[str, object]] = []
    table_results: dict[str, object] = {}
    before_tables = before["tables"]
    after_tables = after["tables"]
    assert isinstance(before_tables, dict) and isinstance(after_tables, dict)
    for table in TABLES:
        left = before_tables[table]
        right = after_tables[table]
        assert isinstance(left, dict) and isinstance(right, dict)
        before_rows = Counter(left.get("canonical_rows", []))
        after_rows = Counter(right.get("canonical_rows", []))
        missing = list((before_rows - after_rows).elements())
        rewritten_or_deleted = len(missing)
        result = {
            "before_count": left["rows"],
            "after_count": right["rows"],
            "before_sha256": left["sha256"],
            "after_sha256": right["sha256"],
            "preserved_rows": rewritten_or_deleted == 0,
            "rewritten_or_deleted": rewritten_or_deleted,
            "appended_rows": max(0, int(right["rows"]) - int(left["rows"])),
        }
        table_results[table] = result
        if left["exists"] != right["exists"] or rewritten_or_deleted:
            violations.append({"table": table, **result})
    if before["schema"] != 52 or after["schema"] != 52:
        violations.append(
            {"schema": {"before": before["schema"], "after": after["schema"]}}
        )
    return {"ok": not violations, "tables": table_results, "violations": violations}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--clone-root", required=True, type=Path)
    parser.add_argument("--core-db", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    root = args.clone_root.resolve(strict=True)
    database = args.core_db.resolve(strict=True)
    if not contained(database, root):
        raise SystemExit("core DB must be contained by clone root")
    output = args.output.resolve(strict=False)
    if not contained(output, root):
        raise SystemExit("output must be contained by clone root")

    database_digest_before = digest_file(database)
    result = {
        "design_id": "D-RB-001",
        "clone_only": True,
        **snapshot_database(database),
    }
    database_digest_after = digest_file(database)
    result["database_sha256_before"] = database_digest_before
    result["database_sha256_after"] = database_digest_after
    result["database_unchanged"] = database_digest_before == database_digest_after

    tables = result["tables"]
    assert isinstance(tables, dict)
    if (
        result["schema"] != 52
        or result["enabled_nostr_agents"] != 2
        or result["enabled_nostr_owners"] != 2
        or not all(tables[table]["exists"] for table in TABLES)
        or database_digest_before != database_digest_after
    ):
        raise SystemExit("clone does not satisfy D-RB-001 preflight invariants")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    output.chmod(0o600)
    print("D-RB-001 clone preflight passed")


if __name__ == "__main__":
    main()
