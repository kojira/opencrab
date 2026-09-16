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
# `--edges no-dev` は normal + build 依存を見て dev-only 依存だけを除外する。
# `--edges normal` だと build 依存を見落とすため、core が gate/SDK を build-
# dependency で引き込む異常を素通りさせてしまう。dev-only の `syn` 等は
# no-dev でも除外されるので、この検査には現れない。
FORBIDDEN='opencrab-gateway|opencrab-discord|opencrab-nostr|opencrab-web-gateway|serenity|serenity-voice-model|songbird'
core_deps="$(cargo tree -p opencrab-core --edges no-dev --prefix none --no-dedupe \
  | sed -E 's/ v[0-9].*//' | sort -u)"
if printf '%s\n' "$core_deps" | grep -qxE "$FORBIDDEN"; then
  echo "R4 FAIL: opencrab-core が gate/SDK クレートに依存している:"
  printf '%s\n' "$core_deps" | grep -xE "$FORBIDDEN"
  exit 1
fi
echo "R4 OK"

# --- R5: SDK（serenity/songbird）は共有層に現れない ---
# R4 と同じ `--edges no-dev`（normal + build、dev-only 除外）で揃える。
SDK='serenity|serenity-voice-model|songbird'
for p in opencrab-gateway opencrab-actions; do
  deps="$(cargo tree -p "$p" --edges no-dev --prefix none --no-dedupe | sed -E 's/ v[0-9].*//' | sort -u)"
  if printf '%s\n' "$deps" | grep -qxE "$SDK"; then
    echo "R5 FAIL: $p に SDK が漏れている:"; printf '%s\n' "$deps" | grep -xE "$SDK"; exit 1
  fi
done
echo "R5(gateway/actions) OK"

# --- R6: shared/server から concrete gateway への逆流を禁止する ---
CONCRETE='opencrab-cli-gateway|opencrab-discord|opencrab-discord-gateway|opencrab-nostr|opencrab-nostr-gateway|opencrab-web-gateway'
for p in opencrab-core opencrab-db opencrab-gateway opencrab-actions opencrab-extgate opencrab-gate-client opencrab-server; do
  deps="$(cargo tree -p "$p" --edges no-dev --prefix none --no-dedupe \
    | sed -E 's/ v[0-9].*//' | sort -u)"
  if printf '%s\n' "$deps" | grep -qxE "$CONCRETE"; then
    echo "R6 FAIL: $p が concrete gateway crate に依存している:"
    printf '%s\n' "$deps" | grep -xE "$CONCRETE"
    exit 1
  fi
done
echo "R6(shared/server -> concrete) OK"

# --- R7: platform実装 -> daemon の逆向き依存を禁止する ---
# daemonがplatform実装を内包する構成ではforward edge自体が無いこともあるため、
# daemon -> platformは必須にせず、存在する場合にも逆辺が無いことだけを固定する。
for pair in 'opencrab-discord opencrab-discord-gateway' 'opencrab-nostr opencrab-nostr-gateway'; do
  set -- $pair
  platform="$1"
  daemon="$2"
  platform_deps="$(cargo tree -p "$platform" --edges no-dev --prefix none --no-dedupe \
    | sed -E 's/ v[0-9].*//' | sort -u)"
  if printf '%s\n' "$platform_deps" | grep -qx "$daemon"; then
    echo "R7 FAIL: $platform が daemon $daemon へ逆向き依存している"
    exit 1
  fi
done
echo "R7(no platform -> daemon reverse edge) OK"
