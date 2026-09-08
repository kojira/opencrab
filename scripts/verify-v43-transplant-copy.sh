#!/usr/bin/env bash
# Compatibility entry point. The current initializer migrates v43 copies through v47.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
exec "$root/scripts/verify-v47-transplant-copy.sh" "$@"
