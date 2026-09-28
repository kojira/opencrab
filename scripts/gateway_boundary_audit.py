#!/usr/bin/env python3
"""Fail-closed static ownership-boundary audit for Issue #1006 S0.

The checked JSON is a line-specific burn-down inventory, not an allowlist.
Every current production occurrence is classified with an owner and expiry;
anything new, moved, duplicated, or removed fails until it is reviewed.
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

SHARED_CRATES = {"actions", "core", "db", "extgate", "gate-client", "gateway", "server"}
FORBIDDEN_GATEWAY_DEPENDENCIES = {
    "opencrab-actions", "opencrab-core", "opencrab-db", "opencrab-discord",
    "opencrab-extgate", "opencrab-gateway", "opencrab-llm",
    "opencrab-llm-types", "opencrab-mcp", "opencrab-nostr",
    "opencrab-server", "opencrab-voice",
}
CONCRETE_VOCABULARY = re.compile(
    r"(?:owner_pubkey|self_pubkey|owner_discord_id|AgentGateway(?:Registry|Lifecycle)?|"
    r"is_known_utterance_op|guild_id|channel_id|discord(?:[_-]?gateway)?|"
    r"nostr(?:[_-]?gateway)?)",
    re.IGNORECASE,
)
SCHEMA_IDENTIFIERS = re.compile(
    r"(?:agent_(?:discord|nostr)_config|channel_config(?:s)?|trusted_user(?:s)?|session_watch(?:es)?)",
    re.IGNORECASE,
)
CONCRETE_ROUTES = re.compile(r"/(?:channel-configs|trusted-users)(?:[/\"]|$)")
DTO_PLATFORM_FIELD = re.compile(
    r"(?:\bpub(?:\([^)]*\))?\s+platform\s*:|"
    r"\bplatform\s*:\s*(?:&(?:'\w+\s+)?str\b|String\b|Option<|Cow<|[A-Z][A-Za-z0-9_:<>]*))"
)
_GUARDED_NAME_EXPR = (
    r"(?:[A-Za-z_][A-Za-z0-9_]*(?:::|\.))*"
    r"(?:gateway_kind|gateway_name|operation_name|platform)\b"
)
NAME_BRANCH = re.compile(
    rf"(?:(?:&\s*|\*\s*)?{_GUARDED_NAME_EXPR}(?:\.as_str\(\))?\s*(?:==|!=|<=|>=|<|>)\s*\"|"
    rf"\bmatch\s+(?:&\s*|\*\s*)?{_GUARDED_NAME_EXPR}(?:\.as_str\(\))?|"
    r"\bdecl\.name\s*(?:==|!=|<=|>=|<|>)\s*\"|\bis_known_utterance_op\s*\()"
)
CORE_PATH = re.compile(r"\b(?:core|legacy)(?:_database|_db)?_path\b", re.IGNORECASE)
DB_CALL = re.compile(r"\b([A-Za-z_][A-Za-z0-9_:]*(?:Store|Db|Database|Connection)|Connection)::(open|open_with_flags|connect)\s*\(")
CORE_STORE_CALL = re.compile(r"\b[A-Za-z_][A-Za-z0-9_:]*(?:Store|Db|Database|Connection)::(?:open|open_with_flags|connect|load)\s*\(")
GATE_ADMIN_PATH = re.compile(r'"(/api/gate-(?:instances|bindings)[^\"]*)"')
HISTORICAL_PARTS = ("/src/schema/migrations/", "/src/schema/tests/")
HISTORICAL_FILES = {
    "crates/db/src/schema/baseline.rs",
    "crates/db/src/schema/migration_tests.rs",
    "crates/db/src/schema/v43_v47.rs",
}
# Five historical tables are created only inside the empty-DB bootstrap transaction
# and removed before its first commit. This is not an exception for live queries.
FRESH_BOOTSTRAP_LEGACY_DROP_IDENTITIES = frozenset({
    ("shared-concrete-schema", "crates/db/src/schema/mod.rs", 119, '"DROP TABLE IF EXISTS channel_config;'),
    ("shared-concrete-schema", "crates/db/src/schema/mod.rs", 120, "DROP TABLE IF EXISTS session_watches;"),
    ("shared-concrete-schema", "crates/db/src/schema/mod.rs", 121, "DROP TABLE IF EXISTS trusted_users;"),
    ("shared-concrete-schema", "crates/db/src/schema/mod.rs", 122, "DROP TABLE IF EXISTS agent_discord_config;"),
    ("shared-concrete-schema", "crates/db/src/schema/mod.rs", 123, 'DROP TABLE IF EXISTS agent_nostr_config;",'),
})
VALID_GATEWAY_DB_OPEN_IDENTITIES = {
    ("gateway-db-open", "crates/discord-gateway/src/daemon.rs", 709, "let store = DiscordStore::open(&config.database_path)?;"),
    ("gateway-db-open", "crates/discord-gateway/src/store.rs", 110, "let conn = Connection::open(path)?;"),
    ("gateway-db-open", "crates/nostr-gateway/src/daemon.rs", 730, "let store = NostrStore::open(&config.database_path)?;"),
    ("gateway-db-open", "crates/nostr-gateway/src/store.rs", 117, "let conn = Connection::open(path)?;"),
    ("gateway-db-open", "crates/web-gateway/src/owner.rs", 39, "let store = Arc::new(Mutex::new(WebStore::open(&config.database_path)?));"),
    ("gateway-db-open", "crates/web-gateway/src/store.rs", 71, "let conn = Connection::open(path)?;"),
}
# These three exact sites are a generic caller-role naming debt, not operation-name routing.
# Exact finding identities prevent a moved or duplicated occurrence from inheriting the deferral.
# Gateway-legacy core state may have exactly these two stopped/offline writers.
# S8 implements `project-core-state`; S10 may later add `destructive-cleanup`.
# A source marker for any other offline writer fails this audit immediately.
GATEWAY_LEGACY_OFFLINE_WRITER_ALLOWLIST = frozenset({
    "project-core-state",
    "destructive-cleanup",
})
OFFLINE_WRITER_MARKER = re.compile(
    r"gateway-legacy offline writer:\s*([a-z][a-z0-9-]*)"
)

DEFERRED_GENERIC_CALLER_ROLE_IDENTITIES = {
    ("shared-concrete-schema", "crates/gateway/src/traits.rs", 19, "TrustedUser,"),
    (
        "shared-concrete-schema",
        "crates/gateway/src/traits.rs",
        46,
        'GatewayCaller::TrustedUser => ("trusted_user", GatewayCallerClass::Trusted),',
    ),
    (
        "shared-concrete-schema",
        "crates/gateway/src/traits.rs",
        81,
        "GatewayCaller::TrustedUser => CallerIdentity::TrustedUser,",
    ),
    (
        "shared-concrete-schema",
        "crates/server/src/heartbeat_instructions.rs",
        112,
        "GatewayCaller::Owner | GatewayCaller::CoAgent { .. } | GatewayCaller::TrustedUser",
    ),
}

VALID_CLASSIFICATIONS = {
    "production-violation",
    "valid-gateway-owned-store",
    "dev-only-qc",
    "transitional-empty-db-bootstrap",
}


class Finding(NamedTuple):
    rule: str
    path: str
    line: int
    snippet: str

    @property
    def key(self) -> tuple[str, str, int, str]:
        return self.rule, self.path, self.line, self.snippet


class RustFunction(NamedTuple):
    path: str
    name: str
    start_line: int
    lines: tuple[tuple[int, str], ...]

    @property
    def body(self) -> str:
        return "\n".join(line for _, line in self.lines)


def _is_offline_gateway_migrator_manifest(path: str) -> bool:
    return path == "crates/gateway-migrate/Cargo.toml"


def _is_gateway_manifest(path: str) -> bool:
    parts = pathlib.PurePosixPath(path).parts
    return len(parts) == 3 and parts[0] == "crates" and parts[1].endswith("-gateway") and parts[2] == "Cargo.toml"


def _dependency_package_name(key: str, value: object) -> str:
    if isinstance(value, dict) and isinstance(value.get("package"), str):
        return value["package"]
    return key


def _is_concrete_gateway_package(name: str) -> bool:
    return (
        name.startswith("opencrab-")
        and name.endswith("-gateway")
        and name not in {"opencrab-gate-client", "opencrab-gateway"}
    )


def _dependency_line(lines: list[str], key: str) -> tuple[int, str]:
    for number, line in enumerate(lines, 1):
        if re.match(rf"\s*{re.escape(key)}(?:\.|\s*=)", line):
            return number, line.strip()
    return 1, lines[0].strip() if lines else ""


def _manifest_findings(path: str, text: str) -> list[Finding]:
    if not path.startswith("crates/") or not path.endswith("/Cargo.toml"):
        return []
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError:
        return []
    lines = text.splitlines()
    is_gateway = _is_gateway_manifest(path)
    findings: list[Finding] = []
    for table_name in ("dependencies", "build-dependencies", "dev-dependencies"):
        dependencies = document.get(table_name, {})
        if not isinstance(dependencies, dict):
            continue
        for key, value in dependencies.items():
            package = _dependency_package_name(key, value)
            line, snippet = _dependency_line(lines, key)
            if table_name != "dev-dependencies" and is_gateway and package in FORBIDDEN_GATEWAY_DEPENDENCIES:
                findings.append(Finding("gateway-production-dependency", path, line, snippet))
            if (
                not is_gateway
                and not _is_offline_gateway_migrator_manifest(path)
                and _is_concrete_gateway_package(package)
            ):
                if table_name != "dev-dependencies":
                    findings.append(Finding("platform-production-gateway-dependency", path, line, snippet))
                elif pathlib.PurePosixPath(path).parts[1] == "server":
                    findings.append(Finding("reviewed-gateway-dev-dependency", path, line, snippet))
                else:
                    findings.append(Finding("unreviewed-gateway-dev-dependency", path, line, snippet))
    return findings


def _strip_rust_comments(lines: Iterable[str]) -> Iterable[tuple[int, str]]:
    """Yield Rust without comments while retaining strings and source lines."""
    block_depth = 0
    for line_number, line in enumerate(lines, 1):
        out: list[str] = []
        i = 0
        quote = False
        escaped = False
        while i < len(line):
            pair = line[i:i + 2]
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
                elif char == '"':
                    quote = False
                i += 1
                continue
            if pair == "//":
                break
            if pair == "/*":
                block_depth = 1
                i += 2
                continue
            if char == '"':
                quote = True
            out.append(char)
            i += 1
        yield line_number, "".join(out)


def _mask_strings(code: str) -> str:
    out: list[str] = []
    quote = False
    escaped = False
    for char in code:
        if quote:
            out.append(" ")
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                quote = False
        else:
            if char == '"':
                quote = True
                out.append(" ")
            else:
                out.append(char)
    return "".join(out)


def _is_test_only_rust_file(path: str, text: str) -> bool:
    return path.endswith(".rs") and text.lstrip().startswith("#![cfg(test)]")


def _production_rust_lines(path: str, text: str) -> Iterable[tuple[int, str]]:
    normalized = f"/{path}"
    file_name = pathlib.PurePosixPath(path).name
    path_parts = pathlib.PurePosixPath(path).parts
    if (
        "/tests/" in normalized
        or file_name == "tests.rs"
        or file_name.endswith("_tests.rs")
        or any(part.endswith("_e2e") for part in path_parts)
    ):
        return
    if any(part in normalized for part in HISTORICAL_PARTS) or path in HISTORICAL_FILES:
        return
    cfg_test_pending = False
    test_depth: int | None = None
    brace_depth = 0
    for line_number, code in _strip_rust_comments(text.splitlines()):
        masked = _mask_strings(code)
        delta = masked.count("{") - masked.count("}")
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
            if "{" in masked:
                test_depth = brace_depth + masked[:masked.index("{") + 1].count("{")
                cfg_test_pending = False
                brace_depth += delta
                if brace_depth < test_depth:
                    test_depth = None
                continue
            if code.strip():
                cfg_test_pending = False
                brace_depth += delta
                continue
        brace_depth += delta
        if code.strip():
            yield line_number, code.strip()


def _shared_production_path(path: str) -> bool:
    parts = pathlib.PurePosixPath(path).parts
    return len(parts) >= 4 and parts[0] == "crates" and parts[1] in SHARED_CRATES and parts[2] == "src" and path.endswith(".rs")


def _gateway_production_path(path: str) -> bool:
    parts = pathlib.PurePosixPath(path).parts
    return len(parts) >= 4 and parts[0] == "crates" and parts[1].endswith("-gateway") and parts[2] == "src" and path.endswith(".rs")


def _normalized_identifier_text(code: str) -> str:
    snake = re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", code)
    return snake.replace("-", "_").lower()


def _shared_source_findings(path: str, text: str) -> list[Finding]:
    findings: list[Finding] = []
    for line_number, code in _production_rust_lines(path, text):
        normalized = _normalized_identifier_text(code)
        if CONCRETE_ROUTES.search(code):
            rule = "shared-concrete-route"
        elif DTO_PLATFORM_FIELD.search(code):
            rule = "shared-platform-dto"
        elif NAME_BRANCH.search(code):
            rule = "shared-gateway-name-branch"
        elif SCHEMA_IDENTIFIERS.search(normalized):
            rule = "shared-concrete-schema"
        elif CONCRETE_VOCABULARY.search(code):
            rule = "shared-concrete-vocabulary"
        else:
            continue
        findings.append(Finding(rule, path, line_number, code))
    return findings


def _rust_use_alias_edges(source: str) -> list[tuple[str, str]]:
    """Return straightforward imported-name -> local-name edges."""
    edges: list[tuple[str, str]] = []
    for match in re.finditer(r"\buse\s+([^;]+);", source, re.DOTALL):
        clause = match.group(1).strip()
        grouped = re.fullmatch(r"(.+?)::\{(.*)\}", clause, re.DOTALL)
        items = grouped.group(2).split(",") if grouped else [clause]
        for item in items:
            item = item.strip()
            if not item or item == "self" or item == "*":
                continue
            renamed = re.fullmatch(
                r"([A-Za-z_][A-Za-z0-9_:]*)(?:\s+as\s+([A-Za-z_][A-Za-z0-9_]*))?",
                item,
            )
            if not renamed:
                continue
            imported = renamed.group(1).rsplit("::", 1)[-1]
            local = renamed.group(2) or imported
            edges.append((imported, local))
    return edges


def _rusqlite_connection_aliases(production: list[tuple[int, str]]) -> set[str]:
    source = "\n".join(code for _, code in production)
    aliases = {"Connection"}
    alias_edges = _rust_use_alias_edges(source)
    for alias, target in re.findall(
        r"\b(?:pub\s+)?type\s+(\w+)\s*=\s*([A-Za-z_][A-Za-z0-9_:]*)\s*;",
        source,
    ):
        alias_edges.append((target.rsplit("::", 1)[-1], alias))
    changed = True
    while changed:
        changed = False
        for imported, local in alias_edges:
            if imported in aliases and local not in aliases:
                aliases.add(local)
                changed = True
    return aliases


def _gateway_source_findings(
    path: str,
    text: str,
    crate_aliases: set[str] | None = None,
) -> list[Finding]:
    findings: list[Finding] = []
    production = list(_production_rust_lines(path, text))
    aliases = crate_aliases or _rusqlite_connection_aliases(production)
    alias_pattern = re.compile(rf"\b(?:{'|'.join(map(re.escape, sorted(aliases)))})::(?:open|open_with_flags)\s*\(")
    for line_number, code in production:
        if CORE_PATH.search(code):
            findings.append(Finding("gateway-core-path", path, line_number, code))
        if DB_CALL.search(code) or alias_pattern.search(code) or "opencrab_db::Db::open(" in code:
            findings.append(Finding("gateway-db-open", path, line_number, code))
        if CORE_PATH.search(code) and CORE_STORE_CALL.search(code):
            findings.append(Finding("gateway-core-store-open", path, line_number, code))
    return findings


def _extract_functions(path: str, text: str) -> list[RustFunction]:
    production = list(_production_rust_lines(path, text))
    functions: list[RustFunction] = []
    index = 0
    while index < len(production):
        line_number, code = production[index]
        match = re.search(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\s*(?:<[^>]*>)?\s*\(", code)
        if not match:
            index += 1
            continue
        name = match.group(1)
        body: list[tuple[int, str]] = []
        depth = 0
        started = False
        cursor = index
        while cursor < len(production):
            number, value = production[cursor]
            body.append((number, value))
            masked = _mask_strings(value)
            if "{" in masked:
                started = True
            if started:
                depth += masked.count("{") - masked.count("}")
                if depth <= 0:
                    break
            cursor += 1
        if started:
            functions.append(RustFunction(path, name, line_number, tuple(body)))
            index = max(index + 1, cursor + 1)
        else:
            index += 1
    return functions


def _balanced_call_arguments(source: str, callee: str) -> list[tuple[int, list[str]]]:
    """Return call positions and top-level balanced arguments."""
    calls: list[tuple[int, list[str]]] = []
    for match in re.finditer(rf"\b{re.escape(callee)}\s*\(", source):
        start = source.find("(", match.start())
        args: list[str] = []
        arg_start = start + 1
        paren_depth = 1
        bracket_depth = 0
        brace_depth = 0
        quote = False
        escaped = False
        index = start + 1
        while index < len(source):
            char = source[index]
            if quote:
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == '"':
                    quote = False
                index += 1
                continue
            if char == '"':
                quote = True
            elif char == "(":
                paren_depth += 1
            elif char == ")":
                paren_depth -= 1
                if paren_depth == 0:
                    args.append(source[arg_start:index].strip())
                    calls.append((match.start(), args))
                    break
            elif char == "[":
                bracket_depth += 1
            elif char == "]":
                bracket_depth -= 1
            elif char == "{":
                brace_depth += 1
            elif char == "}":
                brace_depth -= 1
            elif char == "," and paren_depth == 1 and bracket_depth == 0 and brace_depth == 0:
                args.append(source[arg_start:index].strip())
                arg_start = index + 1
            index += 1
    return calls


def _assignment_expressions(source: str) -> dict[str, list[tuple[int, str]]]:
    assignments: dict[str, list[tuple[int, str]]] = {}
    pattern = re.compile(
        r"(?:\blet\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)"
        r"(?:\s*:[^=;]+)?|\b([A-Za-z_][A-Za-z0-9_]*))\s*=\s*(.*?);",
        re.DOTALL,
    )
    for match in pattern.finditer(source):
        name = match.group(1) or match.group(2)
        assignments.setdefault(name, []).append((match.start(), match.group(3).strip()))
    return assignments


def _expressions_feeding_serve(root: RustFunction) -> list[str]:
    source = "\n".join(code for _, code in root.lines)
    assignments = _assignment_expressions(source)
    expressions: list[str] = []
    pending: list[tuple[str, int]] = []
    for call_position, arguments in _balanced_call_arguments(source, "axum::serve"):
        if len(arguments) >= 2:
            pending.append((arguments[1], call_position))
    visited_variables: set[tuple[str, int]] = set()
    while pending:
        expression, before_position = pending.pop()
        expressions.append(expression)
        for identifier in re.findall(r"\b[A-Za-z_][A-Za-z0-9_]*\b", expression):
            visit = (identifier, before_position)
            if visit in visited_variables or identifier not in assignments:
                continue
            visited_variables.add(visit)
            pending.extend(
                (assigned_expression, assignment_position)
                for assignment_position, assigned_expression in assignments[identifier]
                if assignment_position < before_position
            )
    return expressions


def _public_route_findings(files: Mapping[str, str]) -> list[Finding]:
    functions: list[RustFunction] = []
    for path, text in files.items():
        if _shared_production_path(path):
            functions.extend(_extract_functions(path, text))
    by_name: dict[str, list[RustFunction]] = {}
    for function in functions:
        header = function.body.split("{", 1)[0]
        if re.search(r"->\s*(?:axum::)?Router\b", header):
            by_name.setdefault(function.name, []).append(function)
    roots = [f for f in functions if "axum::serve" in f.body]
    reachable: set[tuple[str, str, int]] = {
        (root.path, root.name, root.start_line) for root in roots
    }
    call_pattern = re.compile(r"(?<!\bfn\s)\b([A-Za-z_][A-Za-z0-9_]*)\s*(?:!\s*)?\(")
    bare_path_pattern = re.compile(
        r"^(?:[A-Za-z_][A-Za-z0-9_]*::)*([A-Za-z_][A-Za-z0-9_]*)$"
    )
    pending: list[RustFunction] = []
    for root in roots:
        for expression in _expressions_feeding_serve(root):
            called_names = set(call_pattern.findall(expression))
            bare_path = bare_path_pattern.fullmatch(expression.strip())
            if bare_path:
                called_names.add(bare_path.group(1))
            for called in called_names:
                pending.extend(by_name.get(called, []))
    while pending:
        function = pending.pop()
        key = (function.path, function.name, function.start_line)
        if key in reachable:
            continue
        reachable.add(key)
        for called in call_pattern.findall(function.body):
            pending.extend(by_name.get(called, []))
    findings: list[Finding] = []
    for function in functions:
        key = (function.path, function.name, function.start_line)
        if key not in reachable:
            continue
        for line_number, code in function.lines:
            if GATE_ADMIN_PATH.search(code):
                findings.append(Finding("public-gate-admin-reachable", function.path, line_number, code))
    return findings


def audit_texts(files: Mapping[str, str]) -> list[Finding]:
    normalized = {
        normalized_path: text
        for path, text in files.items()
        if not _is_test_only_rust_file(
            normalized_path := pathlib.PurePosixPath(path).as_posix(), text
        )
    }
    gateway_alias_sources: dict[str, list[tuple[int, str]]] = {}
    for path, text in normalized.items():
        if _gateway_production_path(path):
            crate = pathlib.PurePosixPath(path).parts[1]
            gateway_alias_sources.setdefault(crate, []).extend(_production_rust_lines(path, text))
    gateway_aliases = {
        crate: _rusqlite_connection_aliases(production)
        for crate, production in gateway_alias_sources.items()
    }
    findings: list[Finding] = []
    for path, text in sorted(normalized.items()):
        if path.endswith("Cargo.toml"):
            findings.extend(_manifest_findings(path, text))
        if _shared_production_path(path):
            findings.extend(_shared_source_findings(path, text))
        elif _gateway_production_path(path):
            crate = pathlib.PurePosixPath(path).parts[1]
            findings.extend(_gateway_source_findings(path, text, gateway_aliases[crate]))
    findings.extend(_public_route_findings(normalized))
    return sorted(set(findings), key=lambda item: item.key)


def gateway_legacy_offline_writer_errors(files: Mapping[str, str]) -> list[str]:
    found: dict[str, list[str]] = {}
    for path, text in sorted(files.items()):
        if not path.endswith(".rs"):
            continue
        for match in OFFLINE_WRITER_MARKER.finditer(text):
            found.setdefault(match.group(1), []).append(path)
    errors = []
    for name, paths in sorted(found.items()):
        if name not in GATEWAY_LEGACY_OFFLINE_WRITER_ALLOWLIST:
            errors.append(f"unapproved gateway-legacy offline writer {name}: {paths}")
        if len(paths) != 1:
            errors.append(f"gateway-legacy offline writer {name} has {len(paths)} markers")
    if "project-core-state" not in found:
        errors.append("approved project-core-state offline writer marker is missing")
    return errors


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
                relative = source.relative_to(root).as_posix()
                text = source.read_text()
                if not _is_test_only_rust_file(relative, text):
                    files[relative] = text
    return files


def _metadata_for(finding: Finding) -> tuple[str, str, str, str]:
    path = finding.path
    if finding.rule == "gateway-production-dependency":
        return "production-violation", "V01", "S5", "remove forbidden normal/build dependency from the concrete daemon"
    if finding.rule == "platform-production-gateway-dependency":
        return "production-violation", "V14", "S5/S9", "remove concrete gateway from non-dev dependency graph; keep reviewed QC edge dev-only"
    if finding.rule == "reviewed-gateway-dev-dependency":
        return "dev-only-qc", "V14", "S11", "retain only while isolated QC needs it and cargo tree --edges no-dev remains free of the daemon"
    if finding.rule == "unreviewed-gateway-dev-dependency":
        return "production-violation", "V14", "S0", "remove or explicitly move reviewed QC dependency to server dev-only scope"
    if finding.key in VALID_GATEWAY_DB_OPEN_IDENTITIES:
        return "valid-gateway-owned-store", "V02", "S5", "retain only while provenance remains the daemon-owned gateway database"
    if finding.rule in {"gateway-core-path", "gateway-core-store-open", "gateway-db-open"}:
        violation = "V03" if "legacy" in finding.snippet.lower() else "V02"
        return "production-violation", violation, "S5/S8", "remove runtime core/legacy path and direct/helper-mediated core database access"
    if finding.rule == "public-gate-admin-reachable":
        return "production-violation", "V07", "S1", "public TCP cannot reach any of the six gate-admin operations"
    if "process_supervisor" in path:
        return "production-violation", "V05", "S5", "move platform-neutral process utility out of shared concrete ownership"
    if "agent_gateway" in path:
        return "production-violation", "V08", "S5", "remove concrete lifecycle registry after daemon-owned lifecycle is live"
    if "timed_fire" in path or "subtask" in path:
        return "production-violation", "V09", "S3", "replace platform-shaped routing with canonical generic binding/session IDs"
    if finding.key in FRESH_BOOTSTRAP_LEGACY_DROP_IDENTITIES:
        return "transitional-empty-db-bootstrap", "V11", "S10", "retain only for historical replay within the fresh empty-database transaction; no live legacy table"
    if finding.key in DEFERRED_GENERIC_CALLER_ROLE_IDENTITIES:
        return "production-violation", "V11", "S5/S10", "rename legacy generic caller-role vocabulary after gateway policy ownership moves"
    if finding.rule == "shared-gateway-name-branch" or "ops_projection" in path or "traits.rs" in path:
        return "production-violation", "V10", "S3", "derive routing solely from dynamic declaration metadata"
    if path.startswith("crates/db/"):
        return "production-violation", "V06", "S8/S10", "project generic state, verify freeze, then guarded cleanup removes concrete schema/query"
    if path.startswith("crates/server/src/api/") or "channel_config" in path or "trusted_users" in path:
        return "production-violation", "V07", "S5/S10", "move administration/state to gateway-owned stores and remove core route/query"
    return "production-violation", "V11", "S3/S5/S10", "remove concrete platform symbol/branch from shared production source"


def cargo_metadata_evidence(root: pathlib.Path) -> tuple[set[tuple[str, str, str]], list[str]]:
    """Classify actual Cargo normal/build edges; dev-only QC edges stay evidence."""
    root = root.resolve()
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=root, check=True, capture_output=True, text=True,
    )
    metadata = json.loads(result.stdout)
    production: set[tuple[str, str, str]] = set()
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
            if kind != "dev" and is_gateway and name in FORBIDDEN_GATEWAY_DEPENDENCIES:
                production.add(("gateway-production-dependency", rel_manifest, name))
            if (
                kind != "dev"
                and not is_gateway
                and package["name"] != "opencrab-gateway-migrate"
                and _is_concrete_gateway_package(name)
            ):
                production.add(("platform-production-gateway-dependency", rel_manifest, name))
            if kind == "dev" and _is_concrete_gateway_package(name):
                if package["name"] == "opencrab-server":
                    dev_only.append(f"{package['name']} -> {name}")
                else:
                    production.add(("unreviewed-gateway-dev-dependency", rel_manifest, name))
    return production, sorted(dev_only)


def baseline_document(findings: list[Finding]) -> dict:
    entries = []
    for finding in findings:
        classification, violation, owner, expiry = _metadata_for(finding)
        entries.append({
            "rule": finding.rule, "path": finding.path, "line": finding.line,
            "snippet": finding.snippet, "classification": classification,
            "violation": violation, "owner_stage": owner, "expires_when": expiry,
        })
    return {
        "schema": 2,
        "purpose": "Issue #1006 S0 line-specific reviewed burn-down; entries are debts or explicit gateway-store provenance, never wildcard exceptions",
        "review": {
            "status": "pending-line-review",
            "finding_count": len(entries),
            "policy": "review every path/line/snippet classification, owner stage, expiry, and gateway-store provenance",
        },
        "entries": entries,
        "coverage": [],
    }


def _entry_key(entry: Mapping[str, object]) -> tuple[str, str, int, str]:
    return str(entry["rule"]), str(entry["path"]), int(entry["line"]), str(entry["snippet"])


def check_baseline(findings: list[Finding], document: Mapping[str, object], root: pathlib.Path | None = None) -> list[str]:
    errors: list[str] = []
    entries = document.get("entries")
    if not isinstance(entries, list):
        return ["baseline entries must be a list"]
    baseline: dict[tuple[str, str, int, str], Mapping[str, object]] = {}
    for entry in entries:
        if not isinstance(entry, dict):
            errors.append("baseline entry is not an object")
            continue
        required = {"rule", "path", "line", "snippet", "classification", "violation", "owner_stage", "expires_when"}
        missing = required - entry.keys()
        if missing:
            errors.append(f"baseline entry missing {sorted(missing)}: {entry}")
            continue
        if entry["classification"] not in VALID_CLASSIFICATIONS:
            errors.append(f"invalid finding classification: {entry}")
        if not str(entry["owner_stage"]).startswith("S") or not str(entry["expires_when"]).strip():
            errors.append(f"baseline entry lacks owner/expiry: {entry}")
        key = _entry_key(entry)
        if key in baseline:
            errors.append(f"duplicate baseline entry: {key}")
        baseline[key] = entry
    review = document.get("review")
    if not isinstance(review, dict) or review.get("status") != "line-by-line-reviewed":
        errors.append("baseline review status must be line-by-line-reviewed")
    elif review.get("finding_count") != len(entries):
        errors.append("baseline review finding_count does not match entries")
    current = {finding.key: finding for finding in findings}
    for key in sorted(current.keys() - baseline.keys()):
        finding = current[key]
        errors.append(f"UNCLASSIFIED {finding.rule} {finding.path}:{finding.line}: {finding.snippet}")
    for key in sorted(baseline.keys() - current.keys()):
        errors.append(f"STALE baseline entry (remove/review it): {key}")
    normative_fields = ("classification", "violation", "owner_stage", "expires_when")
    for key in sorted(current.keys() & baseline.keys()):
        expected = dict(zip(normative_fields, _metadata_for(current[key])))
        entry = baseline[key]
        for field in normative_fields:
            actual = entry.get(field)
            if actual != expected[field]:
                errors.append(
                    f"baseline {field} mismatch for {key}: "
                    f"expected {expected[field]!r}, got {actual!r}"
                )
    coverage = document.get("coverage")
    if not isinstance(coverage, list):
        errors.append("baseline coverage must be a list")
        return errors
    covered = {str(item.get("violation")) for item in coverage if isinstance(item, dict)}
    covered.update(str(entry.get("violation")) for entry in entries if isinstance(entry, dict))
    missing_ids = {f"V{i:02d}" for i in range(1, 17)} - covered
    if missing_ids:
        errors.append(f"V01-V16 coverage missing: {sorted(missing_ids)}")
    coverage_keys: set[tuple[str, str, int, str]] = set()
    for item in coverage:
        if not isinstance(item, dict):
            errors.append("coverage entry is not an object")
            continue
        required = {"violation", "classification", "path", "line", "needle", "owner_stage", "expires_when"}
        absent = required - item.keys()
        if absent:
            errors.append(f"coverage entry missing {sorted(absent)}: {item}")
            continue
        key = (str(item["violation"]), str(item["path"]), int(item["line"]), str(item["needle"]))
        if key in coverage_keys:
            errors.append(f"duplicate coverage anchor: {key}")
        coverage_keys.add(key)
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
                document["coverage"] = json.loads(baseline_path.read_text()).get("coverage", [])
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
    errors.extend(gateway_legacy_offline_writer_errors(repository_texts(root)))
    try:
        metadata_edges, dev_only_edges = cargo_metadata_evidence(root)
    except (OSError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        errors.append(f"cargo metadata evidence failed: {error}")
        metadata_edges, dev_only_edges = set(), []
    source_edges = set()
    for finding in findings:
        if finding.rule not in {
            "gateway-production-dependency",
            "platform-production-gateway-dependency",
            "unreviewed-gateway-dev-dependency",
        }:
            continue
        key = finding.snippet.split("=", 1)[0].strip().split(".", 1)[0]
        manifest = tomllib.loads((root / finding.path).read_text())
        package = key
        for table in ("dependencies", "build-dependencies", "dev-dependencies"):
            if key in manifest.get(table, {}):
                package = _dependency_package_name(key, manifest[table][key])
        source_edges.add((finding.rule, finding.path, package))
    if metadata_edges != source_edges:
        errors.append(f"Cargo metadata/TOML production-edge mismatch: metadata={sorted(metadata_edges)} source={sorted(source_edges)}")
    if errors:
        print("gateway boundary audit FAILED", file=sys.stderr)
        print("\n".join(errors), file=sys.stderr)
        return 1
    counts: dict[str, int] = {}
    for finding in findings:
        counts[finding.rule] = counts.get(finding.rule, 0) + 1
    print(f"gateway boundary audit OK: {len(findings)} classified findings; {counts}")
    print(f"cargo metadata allowed dev-only QC edges: {dev_only_edges}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
