#!/usr/bin/env python3
"""D-RB-001 structural acceptance checks for the recovery candidate."""
from __future__ import annotations

import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def text(path: str) -> str:
    candidate = ROOT / path
    assert candidate.is_file(), f"missing {path}"
    return candidate.read_text()


def main() -> None:
    db_mod = text("crates/db/src/queries/mod.rs")
    assert "mod agent_nostr_config;" in db_mod
    authority = text("crates/server/src/nostr_runner_impl.rs")
    assert "get_agent_nostr_owner_pubkey" in authority
    assert "agent_nostr_config.owner_pubkey" in authority

    server_main = text("crates/server/src/main.rs")
    assert "mod nostr_ignition;" in server_main
    assert "NostrGatewayManager" in server_main
    assert "start_nostr" in server_main

    gateway_main = text("crates/nostr-gateway/src/main.rs")
    assert "DaemonConfig" not in gateway_main
    assert 'first == "daemon"' not in gateway_main
    assert "Placement::load" in gateway_main

    schema = text("crates/db/src/schema/migrations/mod.rs")
    assert "v52" in schema
    assert (ROOT / "crates/db/src/schema/migrations/v52.rs").is_file()

    topology_path = ROOT / "deploy/production-topology.json"
    assert topology_path.is_file(), "missing explicit production topology manifest"
    topology = json.loads(topology_path.read_text())
    assert topology == {
        "version": 1,
        "nostr_lifecycle": "server_owned",
        "nostr_authority": "core_agent_nostr_config",
        "standalone_nostr_daemon": False,
    }

    print("D-RB-001 structural checks passed")


if __name__ == "__main__":
    main()
