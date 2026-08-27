#!/usr/bin/env bash
# Local mirror of .github/workflows/ci.yml
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
# macOS sun_path は約 104 バイト。深い worktree の target 配下だと e2e ソケットが溢れる。
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/opencrab-ci-tgt}"

echo "==> rustc --version"
rustc --version

echo "==> cargo fmt --all -- --check"
cargo fmt --all -- --check

echo "==> cargo clippy --workspace --all-targets --all-features -- -D warnings"
cargo clippy --workspace --all-targets --all-features -- -D warnings

echo "==> cargo build --workspace --all-features"
cargo build --workspace --all-features

echo "==> cargo test --workspace --all-features"
cargo test --workspace --all-features --no-fail-fast

echo "==> baseline capture profile (no ambient features or Nostr config)"
env -u OPENCRAB_SECRET_MASTER_KEY -u OPENCRAB_SKILLS_DIR \
  cargo test -p opencrab-server \
    --no-default-features --features baseline-l2 \
    baseline_l2::tests::

echo "==> scripts/check-no-private-identifiers.sh"
bash scripts/check-no-private-identifiers.sh

echo "==> scripts/check-deps.sh"
bash scripts/check-deps.sh

echo "==> scripts/check-samples-node.sh"
bash scripts/check-samples-node.sh

echo "==> conformance (Rust SUT)"
cargo test -p opencrab-web-gateway --test conformance --all-features

echo "==> conformance (Node SUT)"
OPENCRAB_CONFORMANCE_SUT="$root/samples/node/web-gateway/web-gateway.js" \
  cargo test -p opencrab-web-gateway --test conformance --all-features

echo "==> web: npm ci && npm run build && npm test"
(
  cd web
  npm ci
  npm run build
  npm test
)

echo "LOCAL CI: GREEN"
