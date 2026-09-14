#!/usr/bin/env bash
# Enforce the unconditional 800-line limit for every Rust source file under crates/.

set -euo pipefail
cd "$(dirname "$0")/.."

LIMIT=800

if [ "$#" -ne 0 ]; then
  echo "usage: $0" >&2
  exit 2
fi

violations=""
count=0

while IFS= read -r file; do
  lines=$(wc -l < "$file" | tr -d ' ')
  if [ "$lines" -gt "$LIMIT" ]; then
    count=$((count + 1))
    violations="${violations}$(printf '  %6s  (limit %6s, +%s)  %s' "$lines" "$LIMIT" "$((lines - LIMIT))" "$file")
"
  fi
done <<EOF
$(find crates -type f -name '*.rs' | LC_ALL=C sort)
EOF

if [ "$count" -gt 0 ]; then
  echo "FAIL: ${count} Rust source file(s) exceed ${LIMIT} lines:"
  printf '%s' "$violations"
  exit 1
fi

echo "OK: every Rust source file is at most ${LIMIT} lines."
