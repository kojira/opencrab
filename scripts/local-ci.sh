#!/usr/bin/env bash
# Local mirror of .github/workflows/ci.yml plus TEST-DESIGN §4.1 xfail evaluation.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"

echo "==> cargo fmt --all -- --check"
cargo fmt --all -- --check

echo "==> cargo clippy --workspace --all-targets --all-features -- -D warnings"
cargo clippy --workspace --all-targets --all-features -- -D warnings

echo "==> cargo build --workspace --all-features"
cargo build --workspace --all-features

test_log="$(mktemp)"
test_json="$(mktemp)"
trap 'rm -f "$test_log" "$test_json"' EXIT

echo "==> cargo test --workspace --all-features"
set +e
cargo test --workspace --all-features 2>&1 | tee "$test_log"
test_rc="${PIPESTATUS[0]}"
set -e

echo "==> scripts/check-no-private-identifiers.sh"
bash scripts/check-no-private-identifiers.sh

echo "==> scripts/check-deps.sh"
bash scripts/check-deps.sh

echo "==> web: npm ci && npm run build && npm test"
(
  cd web
  npm ci
  npm run build
  npm test
)

echo "==> xfail evaluation (TEST-DESIGN §4.1)"
python3 - "$test_log" "$test_json" "$root/scripts/expected-fail.toml" "$test_rc" <<'PY'
import json, os, re, subprocess, sys

log_path, json_path, ledger_path, test_rc = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])

def parse_ledger(path):
    entries = []
    current = {}
    with open(path, encoding="utf-8") as fh:
        for raw in fh:
            line = raw.strip()
            if line == "[[xfail]]":
                if current:
                    entries.append(current)
                current = {}
                continue
            if line.startswith("issue"):
                current["issue"] = int(line.split("=", 1)[1].strip())
            elif line.startswith("test"):
                current["test"] = line.split("=", 1)[1].strip().strip('"')
        if current:
            entries.append(current)
    for entry in entries:
        if "issue" not in entry or "test" not in entry:
            raise SystemExit(f"malformed xfail entry: {entry}")
    return entries

def parse_cargo_log(path):
    results = []
    module = None
    running = re.compile(r"Running (?:tests/|unittests )?(?:src/)?([\w./-]+)")
    test_line = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|failed|ignored)")
    with open(path, encoding="utf-8", errors="replace") as fh:
        for raw in fh:
            line = raw.rstrip("\n")
            run = running.search(line)
            if run and ".rs" in run.group(1):
                module = os.path.basename(run.group(1)).removesuffix(".rs")
            match = test_line.match(line)
            if not match:
                continue
            name, status = match.group(1), match.group(2)
            if status == "failed":
                status = "FAILED"
            qualified = name if "::" in name or module is None else f"{module}::{name}"
            results.append({"name": qualified, "bare": name.split("::")[-1], "status": status})
    return results

def names_match(result, ledger_name):
    if result["name"] == ledger_name:
        return True
    if result["bare"] == ledger_name.split("::")[-1] and (
        result["name"] == ledger_name or result["name"].endswith(ledger_name) or ledger_name.endswith(result["bare"])
    ):
        if "::" in result["name"]:
            return result["name"].split("::")[0] == ledger_name.split("::")[0]
        return True
    return False

def issue_state(number):
    import shutil
    if shutil.which("gh") is None:
        print(f"WARNING: gh not available; skipped open-issue check for #{number}", file=sys.stderr)
        return None
    viewed = subprocess.run(
        ["gh", "issue", "view", str(number), "--json", "state", "--jq", ".state"],
        capture_output=True,
        text=True,
    )
    if viewed.returncode != 0:
        print(
            f"WARNING: gh issue view #{number} failed; skipped open-issue check: {viewed.stderr.strip()}",
            file=sys.stderr,
        )
        return None
    return viewed.stdout.strip()

ledger = parse_ledger(ledger_path)
results = parse_cargo_log(log_path)
payload = {"ledger": ledger, "results": results, "cargo_exit": test_rc}
with open(json_path, "w", encoding="utf-8") as fh:
    json.dump(payload, fh, indent=2)
print(f"wrote cargo test JSON summary: {json_path}")

if not results and test_rc != 0:
    raise SystemExit("cargo test failed before producing results (compile/harness); red")

red = []
used = set()
for item in results:
    if item["status"] != "FAILED":
        continue
    matched = None
    for idx, entry in enumerate(ledger):
        if names_match(item, entry["test"]):
            matched = idx
            break
    if matched is None:
        red.append(f"unsanctioned FAIL: {item['name']}")
    else:
        used.add(matched)
        print(f"allowed xfail: {item['name']} (# {ledger[matched]['issue']})")

for idx, entry in enumerate(ledger):
    hits = [item for item in results if names_match(item, entry["test"])]
    if any(item["status"] == "ok" for item in hits) and not any(item["status"] == "FAILED" for item in hits):
        red.append(f"stale xfail PASS: {entry['test']} (#{entry['issue']})")
    state = issue_state(entry["issue"])
    if state is not None and state != "OPEN":
        red.append(f"xfail issue #{entry['issue']} is {state}, not OPEN")

if red:
    print("xfail evaluation RED:")
    for line in red:
        print(f"  - {line}")
    raise SystemExit(1)

print("xfail evaluation GREEN (only sanctioned xfails)")
PY
