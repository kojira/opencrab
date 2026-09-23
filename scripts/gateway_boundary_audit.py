#!/usr/bin/env python3
"""Static gateway/core ownership boundary audit for Issue #1006 S0.

The checked baseline is a burn-down list, not an allowlist: every production
finding is tied to an owner stage and removal criterion, and an unclassified or
new occurrence fails. Tests, comments, and historical migration sources are
excluded structurally rather than by suppressing matching text.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import subprocess
import sys
import tomllib
from typing import Iterable, Mapping, NamedTuple

SHARED_CRATES = {
    "actions",
    "core",
    "db",
    "extgate",
    "gate-client",
    "gateway",
    "server",
}
FORBIDDEN_GATEWAY_DEPENDENCIES = {
    "opencrab-actions",
    "opencrab-core",
    "opencrab-db",
    "opencrab-discord",
    "opencrab-extgate",
    "opencrab-gateway",
    "opencrab-llm",
    "opencrab-llm-types",
    "opencrab-mcp",
    "opencrab-nostr",
    "opencrab-server",
    "opencrab-voice",
}
VOCABULARY = re.compile(
    r"(?:"
    r"agent_(?:discord|nostr)_config|"
    r"channel_configs?|trusted_users|session_watches|"
    r"owner_pubkey|self_pubkey|owner_discord_id|"
    r"AgentGateway(?:Registry|Lifecycle)?|"
    r"is_known_utterance_op|"
    r"guild_id|channel_id|"
    r"discord(?:[_-]?gateway)?|nostr(?:[_-]?gateway)?"
    r")",
    re.IGNORECASE,
)
CORE_SQLITE = re.compile(
    r"(?:core_database_path|legacy_database_path|opencrab_(?:db|core)::|"
    r"use\s+opencrab_(?:db|core)\b)"
)
PUBLIC_ADMIN = re.compile(r"(?:create_router_with_gate|opencrab_extgate::admin_router|\badmin_router\s*\()")
HISTORICAL_PARTS = (
    "/src/schema/migrations/",
    "/src/schema/tests/",
)
HISTORICAL_FILES = {
    "crates/db/src/schema/baseline.rs",
    "crates/db/src/schema/migration_tests.rs",
    "crates/db/src/schema/v43_v47.rs",
}


class Finding(NamedTuple):
    rule: str
    path: str
    line: int
    snippet: str

    @property
    def key(self) -> tuple[str, str, int, str]:
        return self.rule, self.path, self.line, self.snippet


def _is_gateway_manifest(path: str) -> bool:
    parts = pathlib.PurePosixPath(path).parts
    return (
        len(parts) == 3
        and parts[0] == "crates"
        and parts[1].endswith("-gateway")
        and parts[2] == "Cargo.toml"
    )


def _manifest_findings(path: str, text: str) -> list[Finding]:
    if not _is_gateway_manifest(path):
        return []
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError:
        return []
    findings: list[Finding] = []
    lines = text.splitlines()
    for table_name in ("dependencies", "build-dependencies"):
        for dependency in document.get(table_name, {}):
            if dependency not in FORBIDDEN_GATEWAY_DEPENDENCIES:
                continue
            line = next(
                (i for i, value in enumerate(lines, 1) if re.match(rf"\s*{re.escape(dependency)}(?:\.|\s*=)", value)),
                1,
            )
            findings.append(
                Finding("gateway-production-dependency", path, line, lines[line - 1].strip())
            )
    return findings


def _strip_rust_comments(lines: Iterable[str]) -> Iterable[tuple[int, str]]:
    """Yield source with // and /* */ comments removed, preserving strings."""
    block_depth = 0
    for line_number, line in enumerate(lines, 1):
        out: list[str] = []
        i = 0
        quote: str | None = None
        escaped = False
        while i < len(line):
            pair = line[i : i + 2]
            if block_depth:
                if pair == "/*":
                    block_depth += 1
                    i += 2
                elif pair == "*/":
                    block_depth -= 1
                    i += 2
                else:
                    i += 1
                continue
            char = line[i]
            if quote:
                out.append(char)
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == quote:
                    quote = None
                i += 1
                continue
            if pair == "//":
                break
            if pair == "/*":
                block_depth = 1
                i += 2
                continue
            # A single quote commonly begins a Rust lifetime (`'a`), not a
            # quoted region. Double-quoted strings are enough to prevent `//`
            # in URLs from being mistaken for comments.
            if char == '"':
                quote = char
            out.append(char)
            i += 1
        yield line_number, "".join(out)


def _production_rust_lines(path: str, text: str) -> Iterable[tuple[int, str]]:
    normalized = f"/{path}"
    file_name = pathlib.PurePosixPath(path).name
    if (
        "/tests/" in normalized
        or file_name == "tests.rs"
        or file_name.endswith("_tests.rs")
    ):
        return
    if any(part in normalized for part in HISTORICAL_PARTS) or path in HISTORICAL_FILES:
        return

    cfg_test_pending = False
    test_depth: int | None = None
    brace_depth = 0
    for line_number, code in _strip_rust_comments(text.splitlines()):
        delta = code.count("{") - code.count("}")
        if test_depth is not None:
            brace_depth += delta
            if brace_depth < test_depth:
                test_depth = None
            continue
        if "#[cfg(test)]" in code.replace(" ", ""):
            cfg_test_pending = True
            brace_depth += delta
            continue
        if cfg_test_pending:
            if "{" in code:
                test_depth = brace_depth + code[: code.index("{") + 1].count("{")
                cfg_test_pending = False
                brace_depth += delta
                if brace_depth < test_depth:
                    test_depth = None
                continue
            if code.strip():
                # Attribute may precede a one-line test item; exclude that item.
                cfg_test_pending = False
                brace_depth += delta
                continue
        brace_depth += delta
        if code.strip():
            yield line_number, code.strip()


def _shared_production_path(path: str) -> bool:
    parts = pathlib.PurePosixPath(path).parts
    return (
        len(parts) >= 4
        and parts[0] == "crates"
        and parts[1] in SHARED_CRATES
        and parts[2] == "src"
        and path.endswith(".rs")
    )


def _gateway_production_path(path: str) -> bool:
    parts = pathlib.PurePosixPath(path).parts
    return (
        len(parts) >= 4
        and parts[0] == "crates"
        and parts[1].endswith("-gateway")
        and parts[2] == "src"
        and path.endswith(".rs")
    )


def _source_findings(path: str, text: str) -> list[Finding]:
    findings: list[Finding] = []
    if _shared_production_path(path):
        for line_number, code in _production_rust_lines(path, text):
            if VOCABULARY.search(code):
                findings.append(Finding("shared-concrete-vocabulary", path, line_number, code))
            if path.startswith("crates/server/") and PUBLIC_ADMIN.search(code):
                findings.append(Finding("public-gate-admin", path, line_number, code))
    elif _gateway_production_path(path):
        for line_number, code in _production_rust_lines(path, text):
            if CORE_SQLITE.search(code):
                findings.append(Finding("gateway-core-sqlite-open", path, line_number, code))
    return findings


def audit_texts(files: Mapping[str, str]) -> list[Finding]:
    """Return production boundary findings for an in-memory path -> text fixture."""
    findings: list[Finding] = []
    for path, text in sorted(files.items()):
        path = pathlib.PurePosixPath(path).as_posix()
        if path.endswith("Cargo.toml"):
            findings.extend(_manifest_findings(path, text))
        if path.endswith(".rs"):
            findings.extend(_source_findings(path, text))
    return sorted(set(findings), key=lambda item: item.key)


def repository_texts(root: pathlib.Path) -> dict[str, str]:
    files: dict[str, str] = {}
    for crate in sorted(root.joinpath("crates").iterdir()):
        if not crate.is_dir():
            continue
        manifest = crate / "Cargo.toml"
        if manifest.is_file():
            files[manifest.relative_to(root).as_posix()] = manifest.read_text()
        src = crate / "src"
        if src.is_dir():
            for source in src.rglob("*.rs"):
                files[source.relative_to(root).as_posix()] = source.read_text()
    return files


def _metadata_for(finding: Finding) -> tuple[str, str, str]:
    path = finding.path
    if finding.rule == "gateway-production-dependency":
        return "V01", "S5", "remove forbidden normal/build dependency from the concrete daemon"
    if finding.rule == "gateway-core-sqlite-open":
        violation = "V03" if "legacy_database_path" in finding.snippet else "V02"
        return violation, "S5/S8", "remove runtime core/legacy DB field, import, and direct core SQLite access"
    if finding.rule == "public-gate-admin":
        return "V07", "S1", "public TCP has no gate-admin merge, reachability, description, or extension"
    if "process_supervisor" in path:
        return "V05", "S5", "move/rename supervisor utility so shared production source has no concrete vocabulary"
    if "agent_gateway" in path:
        return "V08", "S5", "remove concrete lifecycle registry after daemon-owned lifecycle is live"
    if "timed_fire" in path or "subtask" in path:
        return "V09", "S3", "replace platform-shaped routing fields with canonical generic binding/session IDs"
    if "ops_projection" in path or "operations" in path or "traits.rs" in path:
        return "V10", "S3", "remove operation-name fallback and derive policy solely from declared metadata"
    if path.startswith("crates/db/"):
        return "V06", "S8/S10", "project approved generic state, verify freeze, then guarded cleanup removes concrete core schema/query"
    if path.startswith("crates/server/src/api/") or "channel_config" in path or "trusted_users" in path:
        return "V07", "S5/S10", "move concrete administration/state to gateway-owned stores and remove core route/query"
    return "V11", "S3/S5/S10", "remove concrete platform symbol/branch from shared production source"


def cargo_metadata_evidence(root: pathlib.Path) -> tuple[set[tuple[str, str]], list[str]]:
    """Return forbidden normal/build gateway edges and allowed dev-only QC edges."""
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    )
    metadata = json.loads(result.stdout)
    production: set[tuple[str, str]] = set()
    dev_only: list[str] = []
    for package in metadata["packages"]:
        manifest = pathlib.Path(package["manifest_path"])
        try:
            rel_manifest = manifest.relative_to(root).as_posix()
        except ValueError:
            continue
        is_gateway = _is_gateway_manifest(rel_manifest)
        for dependency in package["dependencies"]:
            name = dependency["name"]
            kind = dependency.get("kind") or "normal"
            if is_gateway and name in FORBIDDEN_GATEWAY_DEPENDENCIES and kind != "dev":
                production.add((rel_manifest, name))
            if (
                kind == "dev"
                and name.startswith("opencrab-")
                and name.endswith("-gateway")
                and package["name"] in {"opencrab-server", "opencrab-web-gateway"}
            ):
                dev_only.append(f"{package['name']} -> {name}")
    return production, sorted(dev_only)


def baseline_document(findings: list[Finding]) -> dict:
    entries = []
    for finding in findings:
        violation, owner, expiry = _metadata_for(finding)
        entries.append(
            {
                "rule": finding.rule,
                "path": finding.path,
                "line": finding.line,
                "snippet": finding.snippet,
                "classification": "production-violation",
                "violation": violation,
                "owner_stage": owner,
                "expires_when": expiry,
            }
        )
    return {
        "schema": 1,
        "purpose": "Issue #1006 S0 line-specific burn-down; entries are debts with mandatory expiry, not permanent exceptions",
        "entries": entries,
        "coverage": [],
    }


def _entry_key(entry: Mapping[str, object]) -> tuple[str, str, int, str]:
    return (
        str(entry["rule"]),
        str(entry["path"]),
        int(entry["line"]),
        str(entry["snippet"]),
    )


def check_baseline(
    findings: list[Finding], document: Mapping[str, object], root: pathlib.Path | None = None
) -> list[str]:
    errors: list[str] = []
    entries = document.get("entries")
    if not isinstance(entries, list):
        return ["baseline entries must be a list"]
    baseline: dict[tuple[str, str, int, str], Mapping[str, object]] = {}
    for entry in entries:
        if not isinstance(entry, dict):
            errors.append("baseline entry is not an object")
            continue
        missing = {
            "rule",
            "path",
            "line",
            "snippet",
            "classification",
            "violation",
            "owner_stage",
            "expires_when",
        } - entry.keys()
        if missing:
            errors.append(f"baseline entry missing {sorted(missing)}: {entry}")
            continue
        if entry["classification"] != "production-violation":
            errors.append(f"invalid production classification: {entry}")
        if not str(entry["owner_stage"]).startswith("S") or not str(entry["expires_when"]).strip():
            errors.append(f"baseline entry lacks owner/expiry: {entry}")
        key = _entry_key(entry)
        if key in baseline:
            errors.append(f"duplicate baseline entry: {key}")
        baseline[key] = entry

    current = {finding.key: finding for finding in findings}
    for key in sorted(current.keys() - baseline.keys()):
        finding = current[key]
        errors.append(
            f"UNCLASSIFIED {finding.rule} {finding.path}:{finding.line}: {finding.snippet}"
        )
    for key in sorted(baseline.keys() - current.keys()):
        errors.append(f"STALE baseline entry (remove/review it): {key}")

    coverage = document.get("coverage")
    if not isinstance(coverage, list):
        errors.append("baseline coverage must be a list")
    else:
        covered = {str(item.get("violation")) for item in coverage if isinstance(item, dict)}
        covered.update(str(entry.get("violation")) for entry in entries if isinstance(entry, dict))
        missing_ids = {f"V{i:02d}" for i in range(1, 17)} - covered
        if missing_ids:
            errors.append(f"V01-V16 coverage missing: {sorted(missing_ids)}")
        for item in coverage:
            if not isinstance(item, dict):
                errors.append("coverage entry is not an object")
                continue
            required = {"violation", "classification", "path", "line", "needle", "owner_stage", "expires_when"}
            absent = required - item.keys()
            if absent:
                errors.append(f"coverage entry missing {sorted(absent)}: {item}")
                continue
            evidence_path = pathlib.Path(str(item["path"]))
            if root is not None:
                evidence_path = root / evidence_path
            if not evidence_path.is_file():
                errors.append(f"coverage path missing: {evidence_path}")
                continue
            evidence_lines = evidence_path.read_text().splitlines()
            line = int(item["line"])
            if line < 1 or line > len(evidence_lines) or str(item["needle"]) not in evidence_lines[line - 1]:
                errors.append(f"coverage anchor changed: {item['violation']} {evidence_path}:{line}")
    return errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=pathlib.Path, default=pathlib.Path(__file__).resolve().parents[1])
    parser.add_argument("--baseline", type=pathlib.Path)
    parser.add_argument("--write-baseline", action="store_true")
    args = parser.parse_args(argv)
    root = args.root.resolve()
    baseline_path = args.baseline or root / "scripts/gateway-boundary-baseline.json"
    findings = audit_texts(repository_texts(root))
    if args.write_baseline:
        document = baseline_document(findings)
        if baseline_path.is_file():
            try:
                previous = json.loads(baseline_path.read_text())
                document["coverage"] = previous.get("coverage", [])
            except json.JSONDecodeError:
                pass
        baseline_path.write_text(json.dumps(document, indent=2, ensure_ascii=False) + "\n")
        print(f"wrote {len(findings)} findings to {baseline_path}; review every changed debt entry")
        return 0
    try:
        document = json.loads(baseline_path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        print(f"gateway boundary audit: cannot read baseline: {error}", file=sys.stderr)
        return 2
    errors = check_baseline(findings, document, root)
    try:
        metadata_edges, dev_only_edges = cargo_metadata_evidence(root)
    except (OSError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        errors.append(f"cargo metadata evidence failed: {error}")
        metadata_edges, dev_only_edges = set(), []
    source_edges = {
        (finding.path, finding.snippet.split(".", 1)[0].split("=", 1)[0].strip())
        for finding in findings
        if finding.rule == "gateway-production-dependency"
    }
    if metadata_edges != source_edges:
        errors.append(
            f"Cargo metadata/TOML production-edge mismatch: metadata={sorted(metadata_edges)} source={sorted(source_edges)}"
        )
    if errors:
        print("gateway boundary audit FAILED", file=sys.stderr)
        print("\n".join(errors), file=sys.stderr)
        return 1
    counts: dict[str, int] = {}
    for finding in findings:
        counts[finding.rule] = counts.get(finding.rule, 0) + 1
    print(f"gateway boundary audit OK: {len(findings)} classified production findings; {counts}")
    print(f"cargo metadata allowed dev-only QC edges: {dev_only_edges}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
