#!/usr/bin/env bash
# Usage: Start Rust backend separately, then run ./dev.sh
# --host 0.0.0.0 が無いとループバックのみで listen し、別端末から届かない（ルートの dev.sh と同じ）。
npx vite --port 3000 --host 0.0.0.0
