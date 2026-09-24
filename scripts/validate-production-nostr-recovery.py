#!/usr/bin/env python3
"""Authentic local D-RB-001 recovery-topology validation.

Builds no mocks into OpenCrab: it starts the packaged server, its two real
server-owned Nostr gateway children, and the packaged Discord/Web gateways.
External services are replaced only at their network/process edges by local
fixtures.  Every database, config, socket, log, and fixture path is contained
under --clone-root.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import sqlite3
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError
from urllib.request import Request, urlopen
import uuid

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location(
    "recovery_preflight", HERE / "preflight-production-nostr-recovery.py"
)
assert SPEC and SPEC.loader
preflight = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(preflight)

AGENTS = ("agent-a", "agent-b")
OWNERS = {"agent-a": "aa" * 32, "agent-b": "bb" * 32}
SELF_KEYS = {"agent-a": "11" * 32, "agent-b": "22" * 32}
MASTER_KEY_B64 = base64.b64encode(bytes(range(32))).decode()


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def wait_for(predicate, label: str, timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    raise RuntimeError(f"timeout waiting for {label}")


def http(method: str, url: str, body: dict | None = None) -> tuple[int, str]:
    data = None if body is None else json.dumps(body).encode()
    request = Request(url, data=data, method=method)
    if data is not None:
        request.add_header("content-type", "application/json")
    try:
        with urlopen(request, timeout=5) as response:
            return response.status, response.read().decode()
    except HTTPError as error:
        return error.code, error.read().decode()


class Process:
    def __init__(self, name: str, command: list[str], cwd: Path, env: dict[str, str]):
        self.name = name
        self.log_path = cwd / "logs" / f"{name}.log"
        self.log_path.parent.mkdir(parents=True, exist_ok=True)
        self.log = self.log_path.open("wb")
        self.process = subprocess.Popen(
            command,
            cwd=cwd,
            env=env,
            stdout=self.log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )

    @property
    def pid(self) -> int:
        return self.process.pid

    def text(self) -> str:
        self.log.flush()
        return self.log_path.read_text(errors="replace")

    def stop(self) -> None:
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=5)
        self.log.close()


class LlmFixture(BaseHTTPRequestHandler):
    requests: list[dict] = []

    def log_message(self, _format: str, *_args) -> None:
        return

    def do_GET(self) -> None:
        if self.path.endswith("/models"):
            self.reply({"object": "list", "data": [{"id": "fixture-model", "object": "model"}]})
        else:
            self.send_error(404)

    def do_POST(self) -> None:
        length = int(self.headers.get("content-length", "0"))
        request = json.loads(self.rfile.read(length) or b"{}")
        type(self).requests.append(request)
        messages = request.get("messages", [])
        has_tool_result = any(message.get("role") == "tool" for message in messages)
        if has_tool_result:
            message = {"role": "assistant", "content": "fixture outbound ok"}
            finish = "stop"
        else:
            message = {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": "owner-proof-ws-list",
                        "type": "function",
                        "function": {"name": "ws_list", "arguments": '{"path":""}'},
                    }
                ],
            }
            finish = "tool_calls"
        self.reply(
            {
                "id": f"fixture-{len(type(self).requests)}",
                "object": "chat.completion",
                "created": 1,
                "model": "fixture-model",
                "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
            }
        )

    def reply(self, value: dict) -> None:
        payload = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def write_config(root: Path, core_port: int, llm_port: int, provider: str) -> None:
    provider_toml = (
        '[llm.providers.codex]\ndefault_model="gpt-5.6"\nmodels=["gpt-5.6"]\n'
        if provider == "codex"
        else f'[llm.providers.fixture]\ntype="openai"\napi_key="fixture"\nbase_url="http://127.0.0.1:{llm_port}/v1"\nmodels=["fixture-model"]\n'
    )
    model = "gpt-5.6" if provider == "codex" else "fixture-model"
    config = f'''[agent]
heartbeat_enabled=false
loop_restart_enabled=false
workspace_path="{root}/data/agents/{{agent_id}}/workspace"
[llm]
default_provider="{provider}"
default_model="{model}"
{provider_toml}
[gateway.rest]
port={core_port}
[database]
path="{root}/data/opencrab.db"
[gate]
listen_socket="{root}/data/extgate.sock"
nostr_ingress="v3"
[llm_log_archive]
enabled=false
[offload_cleanup]
enabled=false
[subtask]
auto_dispatch=false
[skill_consolidation]
enabled=false
[category_maintenance]
enabled=false
[memory_organize]
enabled=false
[memory_declare]
enabled=false
[memory_condense]
enabled=false
'''
    (root / "config").mkdir(parents=True, exist_ok=True)
    (root / "config/default.toml").write_text(config)


def base_env(package: Path, root: Path) -> dict[str, str]:
    env = os.environ.copy()
    env.update(
        {
            "RUST_LOG": "info",
            "OPENCRAB_NOSTR_GATEWAY_BIN": str(package / "nostr-gateway"),
            "OPENCRAB_NOSTARO_BIN": str(root / "fixtures/nostaro"),
            "OPENCRAB_NOSTARO_PUBLISH_LOG": str(root / "fixtures/nostaro-publish.jsonl"),
            "OPENCRAB_SECRET_MASTER_KEY": MASTER_KEY_B64,
            "OPENCRAB_NOSTRGATE_FAKE_WATCH": str(root / "fixtures/nostr.jsonl"),
            "OPENCRAB_DISCORDGATE_FAKE_EVENTS": str(root / "fixtures/discord.jsonl"),
            "OPENCRAB_DISCORDGATE_DRY_RUN": "1",
        }
    )
    env.pop("OPENCRAB_NOSTRGATE_DRY_RUN", None)
    return env


def start_server(package: Path, root: Path, env: dict[str, str], core_port: int, name: str) -> Process:
    process = Process(name, [str(package / "opencrab-server")], root, env)

    def healthy() -> bool:
        if process.process.poll() is not None:
            raise RuntimeError(f"{name} exited: {process.text()[-4000:]}")
        try:
            return http("GET", f"http://127.0.0.1:{core_port}/health")[0] == 200
        except OSError:
            return False

    wait_for(healthy, f"{name} health", 45)
    return process


def seed_clone(database: Path) -> None:
    connection = sqlite3.connect(database)
    try:
        now = "2026-01-01T00:00:00Z"
        for subject, agent in enumerate(AGENTS, start=1):
            connection.execute(
                "INSERT INTO agents(agent_id,name,persona_name,instructions,heartbeat_instructions,created_at,updated_at,subject_id) VALUES(?,?,?,?,?,?,?,?)",
                (agent, agent, agent, "", "", now, now, subject),
            )
            session = f"nostr-{agent}"
            connection.execute(
                "INSERT INTO sessions(id,theme,created_at,updated_at) VALUES(?,?,?,?)",
                (session, session, now, now),
            )
            connection.execute(
                "INSERT INTO agent_sessions(agent_id,session_id) VALUES(?,?)", (agent, session)
            )
            connection.execute(
                "INSERT INTO agent_nostr_config(agent_id,secret_key,relays_json,filter_json,enabled,owner_pubkey,self_pubkey,updated_at) VALUES(?,?,?,?,0,?,?,?)",
                (
                    agent,
                    f"nsec-{agent}",
                    '["wss://fixture.invalid"]',
                    "{}",
                    OWNERS[agent],
                    SELF_KEYS[agent],
                    now,
                ),
            )
            (database.parent / "agents" / agent / "workspace").mkdir(parents=True, exist_ok=True)
        connection.execute(
            "INSERT INTO memory_curated(id,agent_id,category,content,updated_at) VALUES('marker','agent-a','long_term','preserve me',?)",
            (now,),
        )
        connection.execute(
            "INSERT INTO memory_sessions(agent_id,session_id,log_type,content,created_at) VALUES('agent-a','preserved-session','speech','preserve history',?)",
            (now,),
        )
        connection.execute(
            "INSERT INTO daily_log_index_watermark(agent_id,last_indexed_date,updated_at) VALUES('agent-a','2025-12-31',?)",
            (now,),
        )
        connection.execute(
            "INSERT OR REPLACE INTO model_pricing(provider,model,input_price_per_1m,output_price_per_1m,context_window,max_output_tokens,updated_at) VALUES('fixture','fixture-model',0,0,200000,4096,?)",
            (now,),
        )
        connection.commit()
    finally:
        connection.close()


def write_fixtures(root: Path) -> None:
    fixtures = root / "fixtures"
    fixtures.mkdir(parents=True, exist_ok=True)
    (fixtures / "nostr.jsonl").write_text("")
    (fixtures / "discord.jsonl").write_text("")
    nostaro = fixtures / "nostaro"
    nostaro.write_text(
        """#!/usr/bin/env python3
import json, os, pathlib, sys
args=sys.argv[1:]
if 'pubkey' in args:
    secret=os.environ.get('NOSTARO_SECRET_KEY','')
    print(('11' if 'agent-a' in secret else '22')*32)
elif 'following' in args:
    out=next(a.split('=',1)[1] for a in args if a.startswith('--out='))
    pathlib.Path(out).write_text('{"users":[]}')
    print('ok')
elif 'post' in args or 'reply' in args:
    operation='post' if 'post' in args else 'reply'
    record={'operation':operation,'argv':args,'payload':args[-1]}
    out=pathlib.Path(os.environ['OPENCRAB_NOSTARO_PUBLISH_LOG'])
    with out.open('a') as handle:
        handle.write(json.dumps(record,separators=(',',':'))+'\\n')
    print('ok')
else:
    print('ok')
"""
    )
    nostaro.chmod(0o700)


def normalize_secrets(package: Path, root: Path, core_port: int, llm_port: int) -> None:
    # A first schema initialization has no Nostr rows.  A second disabled-Nostr
    # start performs the real at-rest encryption before the validation baseline.
    write_config(root, core_port, llm_port, "codex")
    env = base_env(package, root)
    initial = start_server(package, root, env, core_port, "schema-init")
    initial.stop()
    seed_clone(root / "data/opencrab.db")
    normalizer = start_server(package, root, env, core_port, "secret-normalize")
    normalizer.stop()
    connection = sqlite3.connect(root / "data/opencrab.db")
    try:
        encrypted = connection.execute(
            "SELECT count(*) FROM agent_nostr_config WHERE secret_key LIKE 'enc:v1:%'"
        ).fetchone()[0]
        if encrypted != 2:
            raise RuntimeError(f"expected two encrypted secrets, got {encrypted}")
        connection.execute("UPDATE agent_nostr_config SET enabled=1")
        connection.commit()
    finally:
        connection.close()


def nostr_publish_records(root: Path) -> list[dict]:
    path = root / "fixtures/nostaro-publish.jsonl"
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line]


def append_nostr_events(root: Path) -> None:
    lines = []
    for index, agent in enumerate(AGENTS, start=1):
        lines.append(
            json.dumps(
                {
                    "id": f"{index:02x}" * 32,
                    "pubkey": OWNERS[agent],
                    "npub": None,
                    "note_id": None,
                    "created_at": index,
                    "kind": 1,
                    "content": f"owner proof {agent}",
                    "tags": [["p", SELF_KEYS[agent]]],
                }
            )
        )
    with (root / "fixtures/nostr.jsonl").open("a") as handle:
        handle.write("\n".join(lines) + "\n")


def process_inventory(root: Path, server_pid: int) -> dict[str, object]:
    raw = subprocess.check_output(["ps", "-axo", "pid=,ppid=,command="], text=True)
    rows = []
    parents = {server_pid}
    changed = True
    parsed = []
    for line in raw.splitlines():
        bits = line.strip().split(None, 2)
        if len(bits) == 3:
            parsed.append((int(bits[0]), int(bits[1]), bits[2]))
    while changed:
        changed = False
        for pid, ppid, command in parsed:
            if ppid in parents and pid not in parents:
                parents.add(pid)
                rows.append((pid, ppid, command))
                changed = True
    nostr = [row for row in rows if "nostr-gateway" in row[2]]
    daemon = [row for row in nostr if " daemon " in f" {row[2]} "]
    return {
        "server_pid": server_pid,
        "descendants": [{"pid": p, "ppid": pp, "command_basename": Path(c.split()[0]).name} for p, pp, c in rows],
        "nostr_children": len(nostr),
        "standalone_daemons": len(daemon),
        "admin_uds_present": any(root.rglob("*nostr*admin*.sock")),
    }


def seed_discord_gate(database: Path, root: Path) -> Path:
    agent = "agent-a"
    instance = "dddddddd-dddd-4ddd-8ddd-dddddddddddd"
    address = f"discord-{agent}-500-600"
    config = {
        "agent_id": agent,
        "self_bot_id": "111111111111111111",
        "name": "fixture-discord",
        "access": {"owners": ["222222222222222222"]},
    }
    raw = json.dumps(config, separators=(",", ":")).encode()
    connection = sqlite3.connect(database)
    try:
        connection.execute(
            "INSERT INTO sessions(id,theme,created_at,updated_at) VALUES(?,?,datetime('now'),datetime('now'))",
            (address, address),
        )
        connection.execute(
            "INSERT INTO agent_sessions(agent_id,session_id) VALUES(?,?)", (agent, address)
        )
        connection.execute(
            "INSERT INTO gate_instances(instance_id,kind_id,subject_id,revision,enabled,config_b64,config_digest,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?)",
            (instance, "discord", 1, 1, 1, base64.b64encode(raw).decode(), sha256(raw), 1, 1),
        )
        connection.execute(
            "INSERT INTO gate_bindings(binding_id,instance_id,address,created_at) VALUES(?,?,?,?)",
            ("eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee", instance, address, 1),
        )
        connection.commit()
    finally:
        connection.close()
    placement = {
        "core_socket": str(root / "data/extgate.sock"),
        "attachment_spool_root": str(root / "data/attachments/inbox"),
        "instances": [
            {
                "instance_id": instance,
                "revision": 1,
                "addresses": [address],
                "config_b64": base64.b64encode(raw).decode(),
            }
        ],
    }
    path = root / "placements/discord.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(placement))
    return path


def seed_web_gate(database: Path, root: Path, web_port: int) -> Path:
    instance = "ffffffff-ffff-4fff-8fff-ffffffffffff"
    author = "fixture-web-owner"
    config_bytes = json.dumps({"author_id": author}, separators=(",", ":")).encode()
    connection = sqlite3.connect(database)
    try:
        connection.execute(
            "INSERT INTO gate_instances(instance_id,kind_id,subject_id,revision,enabled,config_b64,config_digest,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?)",
            (instance, "web", 1, 1, 1, base64.b64encode(config_bytes).decode(), sha256(config_bytes), 1, 1),
        )
        connection.commit()
    finally:
        connection.close()
    placement = {
        "http_bind": f"127.0.0.1:{web_port}",
        "core_socket": str(root / "data/extgate.sock"),
        "instances": [{"instance_id": instance, "revision": 1, "agent_id": "agent-a", "author_id": author}],
    }
    path = root / "placements/web.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(placement))
    return path


def owner_tool_proof(database: Path) -> dict[str, int]:
    connection = sqlite3.connect(database)
    try:
        result = {}
        for agent in AGENTS:
            session = f"nostr-{agent}"
            result[agent] = int(
                connection.execute(
                    "SELECT count(*) FROM tool_logs WHERE session_id=? AND tool_name='ws_list' AND outcome='done'",
                    (session,),
                ).fetchone()[0]
            )
        return result
    finally:
        connection.close()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--package-root", required=True, type=Path)
    parser.add_argument("--clone-root", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    package_source = args.package_root.resolve(strict=True)
    root = args.clone_root.resolve(strict=False)
    output = args.output.resolve(strict=False)
    if root.exists():
        shutil.rmtree(root)
    root.mkdir(parents=True, mode=0o700)
    if not preflight.contained(output, root):
        raise SystemExit("output must be contained by clone root")
    package = root / "package"
    package.mkdir()
    for name in ("opencrab-server", "nostr-gateway", "discord-gateway", "web-gateway"):
        source = package_source / name
        if not source.is_file():
            raise SystemExit(f"missing packaged binary: {source}")
        shutil.copy2(source, package / name)
    shutil.copy2(Path(__file__).resolve().parents[1] / "deploy/production-topology.json", package / "production-topology.json")
    topology = json.loads((package / "production-topology.json").read_text())
    if topology != {"version": 1, "nostr_lifecycle": "server_owned", "nostr_authority": "core_agent_nostr_config", "standalone_nostr_daemon": False}:
        raise SystemExit("unexpected package topology")

    write_fixtures(root)
    core_port, llm_port, web_port = free_port(), free_port(), free_port()
    llm = ThreadingHTTPServer(("127.0.0.1", llm_port), LlmFixture)
    thread = threading.Thread(target=llm.serve_forever, daemon=True)
    thread.start()
    processes: list[Process] = []
    try:
        normalize_secrets(package, root, core_port, llm_port)
        write_config(root, core_port, llm_port, "fixture")
        database = root / "data/opencrab.db"
        before = preflight.snapshot_database(database)
        if before["schema"] != 52 or before["enabled_nostr_agents"] != 2 or before["enabled_nostr_owners"] != 2:
            raise RuntimeError("clone baseline does not have schema52 and two Owners")

        env = base_env(package, root)
        server = start_server(package, root, env, core_port, "recovery-server")
        processes.append(server)
        wait_for(lambda: "nostr-gateway running" in server.text(), "two Nostr children logs", 45)
        wait_for(lambda: process_inventory(root, server.pid)["nostr_children"] == 2, "exactly two Nostr children", 30)
        inventory = process_inventory(root, server.pid)
        if inventory["standalone_daemons"] or inventory["admin_uds_present"]:
            raise RuntimeError(f"standalone Nostr daemon/admin UDS present: {inventory}")

        # Seed/start the other packaged gateways only after core is live.  Their
        # rows are append-only validation activity, never baseline rewrites.
        discord_placement = seed_discord_gate(database, root)
        discord = Process("discord-gateway", [str(package / "discord-gateway"), str(discord_placement)], root, env)
        processes.append(discord)
        wait_for(lambda: "discord-gateway running" in discord.text(), "Discord gateway", 30)
        web_placement = seed_web_gate(database, root, web_port)
        web = Process("web-gateway", [str(package / "web-gateway"), str(web_placement)], root, env)
        processes.append(web)
        wait_for(lambda: "web-gateway listening" in web.text(), "Web gateway", 30)

        core_health = http("GET", f"http://127.0.0.1:{core_port}/api/health")
        dashboard_health = http("GET", f"http://127.0.0.1:{core_port}/api/agents")
        model_choices = http("GET", f"http://127.0.0.1:{core_port}/api/llm/model-choices")
        model_pricing = http("GET", f"http://127.0.0.1:{core_port}/api/llm/model-pricing")
        web_create: tuple[int, str] = (503, "not ready")
        def web_ready() -> bool:
            nonlocal web_create
            web_create = http("POST", f"http://127.0.0.1:{web_port}/api/web-conversations", {"agent_id": "agent-a", "name": "fixture"})
            return web_create[0] in (200, 201)
        wait_for(web_ready, "Web instance binding", 30)
        if not all(status in (200, 201) for status, _ in (core_health, dashboard_health, model_choices, model_pricing, web_create)):
            raise RuntimeError(f"HTTP health failed: core={core_health[0]} dashboard={dashboard_health[0]} models={model_choices[0]}/{model_pricing[0]} web={web_create[0]}")

        append_nostr_events(root)
        with (root / "fixtures/discord.jsonl").open("a") as handle:
            handle.write(json.dumps({"id":"700","channel_id":"600","guild_id":"500","author":{"id":"222222222222222222","bot":False,"username":"owner"},"content":"discord fixture health","attachments":[]}) + "\n")
        wait_for(lambda: owner_tool_proof(database) == {"agent-a": 1, "agent-b": 1}, "both Nostr owners executing owner-only ws_list", 45)
        expected_event_ids = {f"{index:02x}" * 32 for index in range(1, 3)}
        wait_for(
            lambda: expected_event_ids
            <= {
                record.get("argv", [])[-2]
                for record in nostr_publish_records(root)
                if len(record.get("argv", [])) >= 2
            },
            "both Owner events published through local nostaro",
            30,
        )
        publishes_by_event = {}
        for record in nostr_publish_records(root):
            argv = record.get("argv", [])
            if len(argv) >= 2 and argv[-2] in expected_event_ids:
                publishes_by_event.setdefault(argv[-2], record)
        publish_records = list(publishes_by_event.values())
        if len(publish_records) != 2 or any(
            record.get("operation") != "reply"
            or record.get("payload") != "fixture outbound ok"
            or not preflight.contained(Path(record["argv"][1]).resolve(), root)
            for record in publish_records
        ):
            raise RuntimeError(f"unexpected local Nostr publish: {publish_records}")
        wait_for(lambda: "fixture outbound ok" in discord.text(), "Discord outbound dry-run", 30)
        owner_proof = owner_tool_proof(database)

        # Graceful shutdown is part of the topology proof.  The final snapshot
        # follows shutdown, as required by D-RB-001.
        for process in reversed(processes):
            process.stop()
        processes.clear()
        after = preflight.snapshot_database(database)
        comparison = preflight.compare_snapshots(before, after)
        # The validation traffic legitimately advances its three pre-created
        # session rows.  Prove every other baseline row is byte-canonical and
        # report these exact, bounded rewrites rather than weakening a table.
        allowed_sessions = {f"nostr-{agent}" for agent in AGENTS} | {"discord-agent-a-500-600"}
        from collections import Counter
        before_session_rows = Counter(before["tables"]["sessions"]["canonical_rows"])
        after_session_rows = Counter(after["tables"]["sessions"]["canonical_rows"])
        rewritten_session_ids = {
            json.loads(row)[0] for row in (before_session_rows - after_session_rows).elements()
        }
        unauthorized_session_ids = rewritten_session_ids - allowed_sessions
        violations = []
        for violation in comparison["violations"]:
            if violation.get("table") != "sessions" or unauthorized_session_ids:
                violations.append(violation)
        comparison["authorized_session_rewrites"] = sorted(rewritten_session_ids & allowed_sessions)
        comparison["unauthorized_session_rewrites"] = sorted(unauthorized_session_ids)
        comparison["violations"] = violations
        comparison["ok"] = not violations
        if not comparison["ok"]:
            raise RuntimeError(f"unauthorized clone data mutation: {violations}")

        result = {
            "design_id": "D-RB-001",
            "clone_only": True,
            "all_paths_under_clone_root": True,
            "package_topology": topology,
            "schema_before": before["schema"],
            "schema_after": after["schema"],
            "checks_before": before["checks"],
            "checks_after": after["checks"],
            "owners": {agent: {"source": "core agent_nostr_config", "owner_only_ws_list_done": owner_proof[agent]} for agent in AGENTS},
            "nostr": {"inbound_owner_events": 2, "outbound_local_publishes": publish_records, "fixture": "local JSONL relay edge"},
            "discord": {"gateway_started": True, "inbound_fixture": True, "outbound_dry_run_observed": True},
            "web_dashboard_model_admin": {"core": core_health[0], "dashboard": dashboard_health[0], "model_choices": model_choices[0], "model_pricing": model_pricing[0], "web_create": web_create[0]},
            "processes": inventory,
            "database_comparison": comparison,
            "llm_fixture_requests": len(LlmFixture.requests),
            "logs": {path.name: str(path.relative_to(root)) for path in (root / "logs").iterdir()},
        }
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
        output.chmod(0o600)
        print(f"D-RB-001 authentic clone topology passed: {output}")
    finally:
        for process in reversed(processes):
            process.stop()
        llm.shutdown()
        llm.server_close()


if __name__ == "__main__":
    main()
