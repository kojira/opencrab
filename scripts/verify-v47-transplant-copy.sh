#!/usr/bin/env bash
# Compatibility command for the current v43 -> v47 synthetic migration checks.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

if [[ -n "${OPENCRAB_REHEARSAL_DB:-}" ]]; then
  echo "OPENCRAB_REHEARSAL_DB is no longer used; verification uses synthetic fixtures" >&2
fi

cargo test -p opencrab-db --lib schema::migration_tests::
echo "v43 -> v47 synthetic migration verification GREEN"
