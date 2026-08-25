#!/usr/bin/env bash
# 載せ替え工程 3（v43）本番コピー検証。
#
# 本番ファイルは開かない（TEST-DESIGN: 読取コピーのみ）。
# ソースは sqlite3 の .backup（mode=ro）だけで読む。パスはリポジトリに書かない。
#
# 使い方:
#   OPENCRAB_REHEARSAL_DB=/path/to/source.db ./scripts/verify-v43-transplant-copy.sh
#
# 検証（設計 §5）:
#   - 既存全表の COUNT と既存列ダイジェストが適用前後で一致（sessions.policy_json は除外）
#   - sessions.policy_json は全行 '{}'
#   - tool_logs / session_watches は 0 行
#   - 適用後の user tables は適用前 ∪ {session_watches, tool_logs} と一致。VIEW sessions は無い
#   - 同一コピー 2 本で適用結果が一致。2 回目は no-op
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

src="${OPENCRAB_REHEARSAL_DB:-}"
if [[ -z "$src" ]]; then
  echo "OPENCRAB_REHEARSAL_DB is required (production db path; opened read-only via .backup)" >&2
  exit 2
fi
if [[ ! -f "$src" ]]; then
  echo "OPENCRAB_REHEARSAL_DB is not a file: $src" >&2
  exit 2
fi

workdir="$(mktemp -d "${TMPDIR:-/tmp}/v43-rehearsal.XXXXXX")"
cleanup() { rm -rf "$workdir"; }
trap cleanup EXIT

echo "==> workdir $workdir"
echo "==> sqlite3 .backup (read-only source)"
sqlite3 "file:${src}?mode=ro" ".backup '${workdir}/pristine.db'"

sqlite3 "${workdir}/pristine.db" ".backup '${workdir}/a.db'"
sqlite3 "${workdir}/pristine.db" ".backup '${workdir}/b.db'"

python3 - "$workdir" <<'PY'
import hashlib
import json
import sqlite3
import sys
from pathlib import Path

work = Path(sys.argv[1])
NEW_TABLES = {"session_watches", "tool_logs"}


def user_tables(conn):
    rows = conn.execute(
        "SELECT name FROM sqlite_master "
        "WHERE type='table' AND name NOT LIKE 'sqlite_%' "
        "ORDER BY name"
    ).fetchall()
    return [r[0] for r in rows]


def table_digest(conn, table, skip_cols):
    cols = conn.execute(
        "SELECT name, pk FROM pragma_table_info(?) ORDER BY cid", (table,)
    ).fetchall()
    selected = [name for name, _pk in cols if name not in skip_cols]
    pk = sorted(((pk, name) for name, pk in cols if pk > 0), key=lambda x: x[0])
    order = ", ".join(f'"{name}"' for _pk, name in pk) if pk else "rowid"
    quoted = ", ".join(f'"{c}"' for c in selected)
    count = conn.execute(f'SELECT COUNT(*) FROM "{table}"').fetchone()[0]
    h = hashlib.sha256()
    for row in conn.execute(f'SELECT {quoted} FROM "{table}" ORDER BY {order}'):
        for name, value in zip(selected, row):
            h.update(name.encode("utf-8"))
            h.update(b"\0")
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
            h.update(cell)
            h.update(b"\1")
        h.update(b"\2")
    return {"count": count, "digest": h.hexdigest(), "columns": selected}


def snapshot(path, skip_new):
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        tables = {}
        for name in user_tables(conn):
            if skip_new and name in NEW_TABLES:
                continue
            skip = ["policy_json"] if name == "sessions" else []
            tables[name] = table_digest(conn, name, skip)
        extra = {}
        names = user_tables(conn)
        extra["user_tables"] = names
        extra["view_sessions"] = conn.execute(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='view' AND name='sessions'"
        ).fetchone()[0]
        if "sessions" in names and "policy_json" in {
            r[1] for r in conn.execute("PRAGMA table_info(sessions)")
        }:
            extra["policy_non_default"] = conn.execute(
                "SELECT COUNT(*) FROM sessions WHERE policy_json IS NULL OR policy_json != '{}'"
            ).fetchone()[0]
        else:
            extra["policy_non_default"] = None
        extra["tool_logs"] = (
            conn.execute("SELECT COUNT(*) FROM tool_logs").fetchone()[0]
            if "tool_logs" in names
            else None
        )
        extra["session_watches"] = (
            conn.execute("SELECT COUNT(*) FROM session_watches").fetchone()[0]
            if "session_watches" in names
            else None
        )
        extra["user_version"] = conn.execute("PRAGMA user_version").fetchone()[0]
        return {"tables": tables, "extra": extra}
    finally:
        conn.close()


before = snapshot(work / "pristine.db", skip_new=False)
(work / "before.json").write_text(json.dumps(before, indent=2, sort_keys=True), encoding="utf-8")
print(f"before tables={len(before['tables'])} user_version={before['extra']['user_version']}")
for name, info in before["tables"].items():
    print(f"  {name}: count={info['count']}")
PY

echo "==> apply v43 to copy A (opencrab_db::schema::initialize)"
OPENCRAB_V43_APPLY_DB="${workdir}/a.db" cargo test -p opencrab-db --lib \
  schema::migration_tests::apply_initialize_to_v43_copy_db -- --exact --nocapture

echo "==> apply v43 to copy B twice (2nd is no-op)"
OPENCRAB_V43_APPLY_DB="${workdir}/b.db" cargo test -p opencrab-db --lib \
  schema::migration_tests::apply_initialize_to_v43_copy_db -- --exact --nocapture
OPENCRAB_V43_APPLY_DB="${workdir}/b.db" cargo test -p opencrab-db --lib \
  schema::migration_tests::apply_initialize_to_v43_copy_db -- --exact --nocapture

python3 - "$workdir" <<'PY'
import json
import sqlite3
import sys
from pathlib import Path

# snapshot() is duplicated here so this process does not depend on the first.
import hashlib

work = Path(sys.argv[1])
NEW_TABLES = {"session_watches", "tool_logs"}


def user_tables(conn):
    rows = conn.execute(
        "SELECT name FROM sqlite_master "
        "WHERE type='table' AND name NOT LIKE 'sqlite_%' "
        "ORDER BY name"
    ).fetchall()
    return [r[0] for r in rows]


def table_digest(conn, table, skip_cols):
    cols = conn.execute(
        "SELECT name, pk FROM pragma_table_info(?) ORDER BY cid", (table,)
    ).fetchall()
    selected = [name for name, _pk in cols if name not in skip_cols]
    pk = sorted(((pk, name) for name, pk in cols if pk > 0), key=lambda x: x[0])
    order = ", ".join(f'"{name}"' for _pk, name in pk) if pk else "rowid"
    quoted = ", ".join(f'"{c}"' for c in selected)
    count = conn.execute(f'SELECT COUNT(*) FROM "{table}"').fetchone()[0]
    h = hashlib.sha256()
    for row in conn.execute(f'SELECT {quoted} FROM "{table}" ORDER BY {order}'):
        for name, value in zip(selected, row):
            h.update(name.encode("utf-8"))
            h.update(b"\0")
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
            h.update(cell)
            h.update(b"\1")
        h.update(b"\2")
    return {"count": count, "digest": h.hexdigest(), "columns": selected}


def snapshot(path, skip_new):
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        tables = {}
        for name in user_tables(conn):
            if skip_new and name in NEW_TABLES:
                continue
            skip = ["policy_json"] if name == "sessions" else []
            tables[name] = table_digest(conn, name, skip)
        extra = {}
        names = user_tables(conn)
        extra["user_tables"] = names
        extra["view_sessions"] = conn.execute(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='view' AND name='sessions'"
        ).fetchone()[0]
        extra["policy_non_default"] = conn.execute(
            "SELECT COUNT(*) FROM sessions WHERE policy_json IS NULL OR policy_json != '{}'"
        ).fetchone()[0]
        extra["tool_logs"] = conn.execute("SELECT COUNT(*) FROM tool_logs").fetchone()[0]
        extra["session_watches"] = conn.execute(
            "SELECT COUNT(*) FROM session_watches"
        ).fetchone()[0]
        extra["user_version"] = conn.execute("PRAGMA user_version").fetchone()[0]
        return {"tables": tables, "extra": extra}
    finally:
        conn.close()


before = json.loads((work / "before.json").read_text(encoding="utf-8"))
after_a = snapshot(work / "a.db", skip_new=True)
after_b = snapshot(work / "b.db", skip_new=True)
(work / "after-a.json").write_text(json.dumps(after_a, indent=2, sort_keys=True), encoding="utf-8")
(work / "after-b.json").write_text(json.dumps(after_b, indent=2, sort_keys=True), encoding="utf-8")

errors = []

if after_a["tables"] != before["tables"]:
    for name in sorted(set(before["tables"]) | set(after_a["tables"])):
        b = before["tables"].get(name)
        a = after_a["tables"].get(name)
        if b != a:
            errors.append(f"existing table diff: {name} before={b} after={a}")

if after_a != after_b:
    errors.append("copy A and copy B (applied twice) did not match")

ea = after_a["extra"]
if ea["user_version"] != 43:
    errors.append(f"user_version={ea['user_version']} want 43")
if ea["policy_non_default"] != 0:
    errors.append(f"sessions.policy_json non-default rows={ea['policy_non_default']}")
if ea["tool_logs"] != 0:
    errors.append(f"tool_logs count={ea['tool_logs']}")
if ea["session_watches"] != 0:
    errors.append(f"session_watches count={ea['session_watches']}")
expected_tables = list(before["extra"]["user_tables"])
for name in ("session_watches", "tool_logs"):
    if name not in expected_tables:
        expected_tables.append(name)
expected_tables.sort()
if ea["user_tables"] != expected_tables:
    errors.append(
        f"user tables != expected closed set got={ea['user_tables']} want={expected_tables}"
    )
if "sessions" not in ea["user_tables"] or "agent_sessions" not in ea["user_tables"]:
    errors.append("sessions / agent_sessions missing")
if ea["view_sessions"]:
    errors.append("VIEW sessions was created")

if errors:
    print("v43 copy verification RED:")
    for line in errors:
        print(f"  - {line}")
    raise SystemExit(1)

print("v43 copy verification GREEN")
print(f"  existing tables={len(after_a['tables'])} (diff zero)")
print(f"  user_version={ea['user_version']}")
print(f"  sessions.policy_json all '{{}}'")
print(f"  tool_logs=0 session_watches=0")
print("  user tables match expected closed set")
print("  two-copy apply (B twice) matched")
PY
