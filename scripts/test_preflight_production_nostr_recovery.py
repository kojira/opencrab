#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
from pathlib import Path
import sqlite3
import tempfile
import unittest

MODULE_PATH = Path(__file__).with_name("preflight-production-nostr-recovery.py")
SPEC = importlib.util.spec_from_file_location("recovery_preflight", MODULE_PATH)
assert SPEC and SPEC.loader
preflight = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(preflight)


class FakeConnection:
    def __init__(self, values: dict[str, list[tuple[str]]]):
        self.values = values

    def execute(self, sql: str):
        return FakeRows(self.values[sql])


class FakeRows:
    def __init__(self, rows: list[tuple[str]]):
        self.rows = rows

    def fetchall(self):
        return self.rows


class RecoveryPreflightTests(unittest.TestCase):
    def test_all_required_memory_and_model_tables_are_covered(self):
        required = {
            "memory_curated",
            "memory_sessions",
            "memory_sessions_fts",
            "conversation_snapshots",
            "memory_index_nodes",
            "memory_category_members",
            "memory_index_watermark",
            "agent_memory_index_config",
            "llm_provider_overrides",
            "model_pricing",
        }
        self.assertLessEqual(required, set(preflight.TABLES))

    def test_integrity_check_failure_is_fatal(self):
        connection = FakeConnection(
            {
                "PRAGMA quick_check": [("ok",)],
                "PRAGMA integrity_check": [("row 7 missing",)],
            }
        )
        with self.assertRaisesRegex(ValueError, "integrity_check failed"):
            preflight.integrity_checks(connection)

    def test_snapshot_comparison_allows_append_only_rows(self):
        before = self.snapshot_with_rows(["old"])
        after = self.snapshot_with_rows(["old", "new"])
        comparison = preflight.compare_snapshots(before, after)
        self.assertTrue(comparison["ok"])
        self.assertEqual(comparison["tables"]["agents"]["appended_rows"], 1)

    def test_snapshot_comparison_rejects_rewrite_or_deletion(self):
        before = self.snapshot_with_rows(["old"])
        after = self.snapshot_with_rows(["rewritten"])
        comparison = preflight.compare_snapshots(before, after)
        self.assertFalse(comparison["ok"])
        self.assertEqual(
            comparison["tables"]["agents"]["rewritten_or_deleted"], 1
        )

    def test_snapshot_runs_both_database_checks(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "clone.db"
            connection = sqlite3.connect(path)
            connection.executescript(
                "PRAGMA user_version=52;"
                "CREATE TABLE agent_nostr_config(enabled INTEGER, owner_pubkey TEXT);"
            )
            for table in preflight.TABLES:
                if table != "agent_nostr_config":
                    connection.execute(f'CREATE TABLE "{table}" (value TEXT)')
            connection.commit()
            connection.close()
            snapshot = preflight.snapshot_database(path)
            self.assertEqual(snapshot["checks"]["quick_check"], ["ok"])
            self.assertEqual(snapshot["checks"]["integrity_check"], ["ok"])

    @staticmethod
    def snapshot_with_rows(rows: list[str]):
        tables = {}
        for table in preflight.TABLES:
            table_rows = rows if table == "agents" else []
            tables[table] = {
                "exists": True,
                "rows": len(table_rows),
                "sha256": "digest",
                "canonical_rows": table_rows,
            }
        return {"schema": 52, "tables": tables}


if __name__ == "__main__":
    unittest.main()
