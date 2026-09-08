#!/usr/bin/env bash
# Compatibility command for the current v43 -> v47 synthetic migration checks.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

cargo test -p opencrab-db --lib schema::migration_tests::
echo "v43 -> v47 synthetic migration verification GREEN"
