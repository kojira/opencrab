#!/usr/bin/env python3
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]


class StrictSeparationParity(unittest.TestCase):
    def read(self, relative: str) -> str:
        path = ROOT / relative
        return path.read_text() if path.exists() else ""

    def test_delivery_guarantee_api_is_not_part_of_separation(self) -> None:
        production = "\n".join(
            self.read(path)
            for path in [
                "crates/extgate/src/operations.rs",
                "crates/extgate/src/protocol.rs",
                "crates/extgate/src/registry.rs",
                "crates/extgate/src/listen/hello.rs",
                "crates/extgate/src/operation_calls.rs",
                "crates/gate-client/src/wire.rs",
                "crates/gate-client/src/client/state_api.rs",
            ]
        )
        for forbidden in [
            "DeliveryGuarantee",
            "delivery_guarantee",
            "required_delivery_guarantee",
        ]:
            self.assertNotIn(forbidden, production)

    def test_s6_current_relationship_rechecks_are_not_part_of_separation(self) -> None:
        self.assertFalse((ROOT / "crates/core/src/authorization.rs").exists())
        self.assertFalse((ROOT / "crates/server/src/authorization.rs").exists())
        self.assertFalse((ROOT / "crates/db/src/schema/migrations/v56.rs").exists())
        production = "\n".join(
            self.read(path)
            for path in [
                "crates/actions/src/agent_runtime.rs",
                "crates/actions/src/run_request.rs",
                "crates/actions/src/subtask/dispatcher.rs",
                "crates/core/src/engine/skill_engine.rs",
                "crates/core/src/engine/skill_engine/run.rs",
                "crates/server/src/process/mod.rs",
                "crates/server/src/process/wiring.rs",
            ]
        )
        for forbidden in [
            "AuthorizationBoundary",
            "AuthorizationCheck",
            "with_relationship_authority",
            "co_agent_relationship_is_current",
        ]:
            self.assertNotIn(forbidden, production)

    def test_s7_durable_delivery_strengthening_is_not_part_of_separation(self) -> None:
        self.assertFalse((ROOT / "crates/db/src/schema/migrations/v57.rs").exists())
        self.assertFalse((ROOT / "crates/gate-client/src/emission.rs").exists())
        self.assertFalse((ROOT / "crates/nostr-gateway/src/run/delivery.rs").exists())
        production = "\n".join(
            self.read(path)
            for path in [
                "crates/extgate/src/delivery.rs",
                "crates/extgate/src/close.rs",
                "crates/discord-gateway/src/post.rs",
                "crates/discord-gateway/src/run.rs",
                "crates/nostr-gateway/src/post.rs",
                "crates/nostr-gateway/src/run.rs",
            ]
        )
        for forbidden in [
            "emission_ledger",
            "prepared_protocol_digest",
            "prepare_signed_event",
            "prepare_say_requests",
            "delivery_chunk_nonce",
            "delivery_ack",
        ]:
            self.assertNotIn(forbidden, production)


if __name__ == "__main__":
    unittest.main()
