#!/usr/bin/env python3
"""Clone-only preflight for D-RB-001. Never opens a path outside --clone-root."""
from __future__ import annotations

import argparse
import hashlib
import json
import sqlite3
from pathlib import Path

TABLES = (
    "agents",
    "sessions",
    "memory_sessions",
    "tool_logs",
    "llm_logs",
    "agent_nostr_config",
    "trusted_users",
    "trusted_co_agents",
    "gate_instances",
    "gate_bindings",
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


def digest_table(connection: sqlite3.Connection, table: str) -> dict[str, object]:
    exists = connection.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (table,)
    ).fetchone()
    if not exists:
        return {"exists": False, "rows": 0, "sha256": None}
    digest = hashlib.sha256()
    rows = 0
    for row in connection.execute(f'SELECT * FROM "{table}" ORDER BY rowid'):
        digest.update(json.dumps(row, ensure_ascii=False, separators=(",", ":"), default=str).encode())
        digest.update(b"\n")
        rows += 1
    return {"exists": True, "rows": rows, "sha256": digest.hexdigest()}


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
    connection = sqlite3.connect(f"file:{database}?mode=ro", uri=True)
    try:
        schema = int(connection.execute("PRAGMA user_version").fetchone()[0])
        quick = connection.execute("PRAGMA quick_check").fetchone()[0]
        owners = int(
            connection.execute(
                "SELECT count(*) FROM agent_nostr_config "
                "WHERE enabled=1 AND trim(owner_pubkey)<>''"
            ).fetchone()[0]
        )
        enabled = int(
            connection.execute(
                "SELECT count(*) FROM agent_nostr_config WHERE enabled=1"
            ).fetchone()[0]
        )
        result = {
            "design_id": "D-RB-001",
            "clone_only": True,
            "schema": schema,
            "quick_check": quick,
            "enabled_nostr_agents": enabled,
            "enabled_nostr_owners": owners,
            "tables": {table: digest_table(connection, table) for table in TABLES},
        }
    finally:
        connection.close()
    database_digest_after = digest_file(database)
    result["database_sha256_before"] = database_digest_before
    result["database_sha256_after"] = database_digest_after
    result["database_unchanged"] = database_digest_before == database_digest_after

    if (
        schema != 52
        or quick != "ok"
        or enabled != 2
        or owners != 2
        or database_digest_before != database_digest_after
    ):
        raise SystemExit("clone does not satisfy D-RB-001 preflight invariants")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    output.chmod(0o600)
    print("D-RB-001 clone preflight passed")


if __name__ == "__main__":
    main()
