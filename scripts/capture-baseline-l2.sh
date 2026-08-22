#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output=${1:-"$repo_dir/baseline/l2/opencrab-l2.json"}

cd "$repo_dir"
exec cargo run --quiet -p opencrab-server --bin baseline-l2 --no-default-features --features baseline-l2 -- \
  "$repo_dir/baseline/l1/opencrab-l1.json" \
  "$repo_dir/baseline/l2/scenarios.json" \
  "$output"
