#!/usr/bin/env bash
set -euo pipefail

# クレート境界の検査（依存の向き）。
#
# これは「ビルドが通るか」を見る検査ではない。ビルドは gate/SDK クレートを
# core が巻き込んでいても（依存の向きが逆でも）通ってしまう。ここで見るのは
# `cargo tree` が返す "依存ツリーの内容" — つまり core が何を引き込んでいるか
# そのものである。向きの逆転はコンパイル可否には現れないので、ツリーを直接
# 検査するしかない。
#
# 守るもの: DESIGN.md §2.2 / design-plugin-architecture.md の「コア（gateway
# 非依存層）は transport・SDK を知らない」という依存の向き。core が gate/SDK
# クレートに依存し始めた瞬間に CI を赤にして、境界の逆流を入口で止める。

# --- R4: core は gate/SDK クレートに依存しない ---
FORBIDDEN='opencrab-gateway|opencrab-discord|opencrab-nostr|opencrab-web-gateway|serenity|serenity-voice-model|songbird'
core_deps="$(cargo tree -p opencrab-core --edges normal --prefix none --no-dedupe \
  | sed -E 's/ v[0-9].*//' | sort -u)"
if printf '%s\n' "$core_deps" | grep -qxE "$FORBIDDEN"; then
  echo "R4 FAIL: opencrab-core が gate/SDK クレートに依存している:"
  printf '%s\n' "$core_deps" | grep -xE "$FORBIDDEN"
  exit 1
fi
echo "R4 OK"

# --- R5（SDK が共有層に現れない）は段階 1 で有効化する ---
