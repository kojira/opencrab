#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_dir"
exec cargo run --quiet -p opencrab-server --bin baseline-l1 --no-default-features --features baseline-l1 -- "$repo_dir/baseline/l1/opencrab-l1.json"
