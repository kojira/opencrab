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

# --- R5 完成版: ゲートを外した構成でゲート/SDK が依存ツリーに出ない（PR-1B） ---
# server から 3 つのゲート（discord / nostr / web）を全て外した構成では、ゲートクレート
# 本体と各 SDK（serenity / songbird）が依存ツリーに 1 つも現れないことを固定する。R4/R5 と
# 同じ `--edges no-dev`（normal + build、dev-only 除外）で揃える。
GATES='serenity|serenity-voice-model|songbird|opencrab-discord|opencrab-nostr|opencrab-web-gateway'
bare="$(cargo tree -p opencrab-server --no-default-features --edges no-dev --prefix none --no-dedupe \
  | sed -E 's/ v[0-9].*//' | sort -u)"
if printf '%s\n' "$bare" | grep -qxE "$GATES"; then
  echo "R5 FAIL: server(no gates) にゲート/SDK が残っている:"; printf '%s\n' "$bare" | grep -xE "$GATES"; exit 1
fi
echo "R5(server no-default) OK"

# --- R6: feature の組み合わせでビルドできる（PR-1B: 3 ゲートの全マトリクス） ---
# CI では build/test（--all-features）の後ろで走る（check-deps.sh の呼び出し位置）。
# 各ゲートを個別に付けた構成と、全部外した構成・既定（全部入り）がそれぞれビルドできること。
cargo build -p opencrab-gateway
cargo build -p opencrab-server --no-default-features
for f in discord nostr web; do
  cargo build -p opencrab-server --no-default-features --features "$f"
done
cargo build -p opencrab-server
echo "R6(full matrix) OK"
