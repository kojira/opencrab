#!/usr/bin/env bash
set -euo pipefail

# 新 runtime の依存境界を cargo metadata ではなく実際の normal/build 依存ツリーで検査する。
# dev-only の E2E 補助依存は runtime link に入らないため除外する。

tree_packages() {
  cargo tree -p "$1" --edges no-dev --prefix none --no-dedupe \
    | sed -E 's/ v[0-9].*//' \
    | sort -u
}

assert_absent() {
  local package="$1"
  local forbidden="$2"
  local label="$3"
  local deps
  deps="$(tree_packages "$package")"
  if printf '%s\n' "$deps" | grep -qxE "$forbidden"; then
    echo "$label FAIL: $package の runtime 依存に禁止パッケージがある:"
    printf '%s\n' "$deps" | grep -xE "$forbidden"
    exit 1
  fi
  echo "$label OK: $package"
}

# port は最下層。store/engine/social-runtime/plugd/app への逆流を許さない。
assert_absent \
  opencrab-port \
  'opencrab-(store|engine|social-runtime|plugd|app|web-gate|nostr-gate)' \
  R1

# social-runtime は transport/gate と、その SDK を知らない。
assert_absent \
  opencrab-social-runtime \
  'opencrab-(plugd|app|web-gate|nostr-gate)|tokio-tungstenite|tungstenite|secp256k1|bech32' \
  R2

# 独立 gate は workspace 内 runtime crate の型へ link しない。
for package in opencrab-web-gate opencrab-nostr-gate; do
  assert_absent "$package" 'opencrab-(port|store|engine|social-runtime|plugd|app)' R3
done

# 旧 runtime package が workspace/lockfile に再び現れないことを固定する。
legacy='opencrab-(core|llm|llm-types|gateway|actions|db|discord|nostr|web-gateway|mcp|server|cli|voice)'
workspace_packages="$(cargo metadata --no-deps --format-version 1 \
  | tr ',' '\n' \
  | sed -nE 's/.*"name":"([^"]+)".*/\1/p')"
if printf '%s\n' "$workspace_packages" | grep -qxE "$legacy"; then
  echo "R4 FAIL: 旧 runtime package が workspace に残っている:"
  printf '%s\n' "$workspace_packages" | grep -xE "$legacy"
  exit 1
fi
echo "R4 OK: legacy runtime packages absent"
