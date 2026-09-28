#!/usr/bin/env python3
"""Assertion-level RED contract for Issue #1006 S8.

The behavioral cases deliberately stop at the missing production package today.  Once
`opencrab-gateway-migrate` exists, each named assertion must be replaced by/invoked
through that package's integration fixture before the corresponding GREEN is claimed.
This file is tests/fixtures only; it contains no migration implementation.
"""

from __future__ import annotations

import hashlib
import json
import pathlib
import struct
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
FIXTURES = pathlib.Path(__file__).with_name("fixtures") / "issue_1006_s8"
MIGRATOR = ROOT / "crates" / "gateway-migrate"


def lp(value: bytes) -> bytes:
    return struct.pack(">Q", len(value)) + value


def source_fingerprint(table: str, columns: list[tuple[str, object]]) -> str:
    encoded = bytearray(b"opencrab/s8/source-row/v1\0")
    encoded += lp(table.encode())
    encoded += struct.pack(">Q", 57)
    encoded += struct.pack(">I", len(columns))
    for name, value in columns:
        encoded += lp(name.encode())
        if value is None:
            encoded += b"\x00"
        else:
            encoded += b"\x01"
            if isinstance(value, int):
                raw = str(value).encode()
            elif isinstance(value, bytes):
                raw = value
            else:
                raw = str(value).encode()
            encoded += lp(raw)
    return hashlib.sha256(encoded).hexdigest()


class S8PublishedContractVectors(unittest.TestCase):
    def test_published_source_fingerprints_are_exact(self) -> None:
        vectors = {
            "trusted_users": (
                [
                    ("id", "tu-1"), ("user_id", "42"), ("agent_id", "agent-a"),
                    ("permission", "co-agent"), ("created_by", "owner"),
                    ("created_at", "2026-01-01T00:00:00Z"),
                    ("display_name", "Crab"), ("platform", "rest"),
                ],
                "8addb00d52490e57c4ae221cc3383599f563af00527e786ebe72107ba8b9a6f6",
            ),
            "channel_config": (
                [
                    ("channel_id", "chan-1"), ("agent_id", "agent-a"),
                    ("guild_id", "guild-1"), ("channel_name", "General"),
                    ("readable", 1), ("writable", 0), ("whitelisted", 1),
                    ("heartbeat_enabled", 1), ("heartbeat_interval_secs", None),
                    ("heartbeat_instructions", "Ping"),
                    ("updated_at", "2026-01-01T00:00:00Z"),
                ],
                "38c5adc34e58b2fb8a26b851dc6034d8fb284ac8b4b81b1b4e6963b3fe013366",
            ),
            "session_watches": (
                [
                    ("id", 7), ("session_id", "session-a"), ("agent_id", "agent-a"),
                    ("interval_secs", 600), ("filter_json", '{"authors":["abc"]}'),
                    ("created_at", "2026-01-01T00:00:00Z"),
                ],
                "1d1b6f63fd285176be583a8e0f9bf6eff9341ec58e4feeb767522053f9091300",
            ),
            "agent_discord_config": (
                [
                    ("agent_id", "agent-a"), ("bot_token", "test-token"),
                    ("owner_discord_id", "42"), ("enabled", 1),
                    ("updated_at", "2026-01-01T00:00:00Z"), ("bot_user_id", "99"),
                ],
                "4c07ecde5c01d1ccd062c72f0a1a22ae487466a1f0eb7804a9863e991a799a84",
            ),
            "agent_nostr_config": (
                [
                    ("agent_id", "agent-a"), ("secret_key", "test-secret"),
                    ("relays_json", '["wss://relay.example"]'),
                    ("filter_json", '{"kinds":[1]}'), ("enabled", 1),
                    ("updated_at", "2026-01-01T00:00:00Z"),
                    ("owner_pubkey", "owner-pub"), ("self_pubkey", "self-pub"),
                ],
                "80ba03a96109ec47692292bd1ccb52bcb5b4ccd595d887381e15823765a6221a",
            ),
        }
        for table, (columns, expected) in vectors.items():
            with self.subTest(table=table):
                self.assertEqual(source_fingerprint(table, columns), expected)

    def test_approval_fixtures_pin_strict_shape_order_and_version(self) -> None:
        approval = json.loads((FIXTURES / "approval-v1.json").read_text())
        self.assertEqual(list(approval), [
            "version", "operation_id", "created_at", "core_user_version",
            "source_core_sha256", "destinations", "identity_dispositions",
            "channel_edges", "watch_edges", "credential_sources",
        ])
        self.assertEqual(approval["core_user_version"], 57)
        self.assertEqual(
            [(x["kind_id"], x["path_id"]) for x in approval["destinations"]],
            sorted((x["kind_id"], x["path_id"]) for x in approval["destinations"]),
        )
        unknown = json.loads((FIXTURES / "approval-unknown-field.json").read_text())
        self.assertIn("unexpected", unknown)
        unsorted = json.loads((FIXTURES / "approval-unsorted.json").read_text())
        keys = [(x["kind_id"], x["path_id"]) for x in unsorted["destinations"]]
        self.assertNotEqual(keys, sorted(keys))


class S8ProductionSeamsRed(unittest.TestCase):
    def _require_migrator(self, seam: str) -> None:
        manifest = MIGRATOR / "Cargo.toml"
        main = MIGRATOR / "src" / "main.rs"
        workspace = (ROOT / "Cargo.toml").read_text()
        self.assertTrue(
            manifest.is_file() and main.is_file() and '"crates/gateway-migrate"' in workspace,
            f"S8 production seam absent for {seam}: opencrab-gateway-migrate is not a workspace executable",
        )

    def test_migrator_executable_is_offline_and_present(self) -> None:
        self._require_migrator("offline executable")

    def test_strict_manifests_and_current_schema_refusal(self) -> None:
        self._require_migrator("strict approval/verification JSON, v54/newer-than-57 refusal, and strict v57 acceptance")

    def test_required_and_optional_table_shapes_fail_closed(self) -> None:
        self._require_migrator("required-table absence and optional concrete-table exact-shape refusal")

    def test_dispositions_cover_rest_gateway_unknown_and_extgate_fanout(self) -> None:
        self._require_migrator("rest-only core disposition, explicit unknown/extgate gateway fanout, and zero-unmapped proof")

    def test_rest_permission_vectors_preserve_owner_coagent_unknown_and_web_behavior(self) -> None:
        self._require_migrator("REST owner/co-agent/unknown permission vectors and web exclusion")

    def test_mapping_covers_discord_precedence_nostr_watch_and_web_refusal(self) -> None:
        self._require_migrator("Discord exact/global mapping, Nostr watch equality, and Web cardinality/role refusal")

    def test_credentials_reject_conflict_and_accept_equal_existing_envelope(self) -> None:
        self._require_migrator("credential candidate conflict and existing-envelope equality")

    def test_import_prevalidates_all_destinations_backs_up_and_keeps_core_read_only(self) -> None:
        self._require_migrator("all-destination prevalidation, matched SQLite backups, and read-only core import")

    def test_partial_artifact_rerun_is_operation_bound_and_conflict_safe(self) -> None:
        self._require_migrator("immutable partial artifact, lost-response rerun, and overlapping-operation refusal")

    def test_project_core_state_is_atomic_idempotent_and_mismatch_closed(self) -> None:
        self._require_migrator("atomic project-core-state marker, transaction rollback, immutable read-only already_applied, and mismatch refusal")

    def test_project_core_state_handles_heartbeat_new_existing_and_conflict(self) -> None:
        self._require_migrator("heartbeat new/existing/conflict projection through the S4 seam")

    def test_migration_preserves_ids_history_bindings_and_deliveries(self) -> None:
        self._require_migrator("byte/logical preservation of IDs, history, bindings, and deliveries")

    def test_rest_runtime_uses_api_principals_with_historical_permission_mapping(self) -> None:
        source = (ROOT / "crates/server/src/api/agents_messages.rs").read_text()
        self.assertTrue(
            "get_api_principal" in source,
            "REST production seam must resolve through api_principals after S8",
        )
        self.assertTrue(
            'get_trusted_user(&conn, "rest"' not in source,
            "REST production seam must stop querying legacy trusted_users",
        )

    def test_exactly_two_gateway_legacy_direct_writers_are_audited(self) -> None:
        audit = (ROOT / "scripts/gateway_boundary_audit.py").read_text()
        for required in ["project-core-state", "destructive-cleanup", "offline writer"]:
            self.assertIn(
                required,
                audit,
                f"writer audit must recognize exactly the approved gateway-legacy writer: {required}",
            )


if __name__ == "__main__":
    unittest.main()
