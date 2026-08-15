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

# --- R6: feature の組み合わせでビルド＋（外した構成は）テストできる（PR-1B: 3 ゲートの全マトリクス） ---
# CI では build/test（--all-features）の後ろで走る（check-deps.sh の呼び出し位置）。
# 各ゲートを個別に付けた構成と、全部外した構成・既定（全部入り）がそれぞれビルドできること。
#
# #654: 「ビルドは通るがテストが落ちる」を CI が捕まえられるよう、**全ゲートを外した構成
# （--no-default-features）ではテストまで回す**。ゲートの feature 化（#651）でツール定義や
# ルート・発火経路が feature 依存になったのに、それを前提にしたテストが feature 非依存のまま
# 残ると、外した構成のテストを誰も実行せず割れ窓化する（通常の CI テストは --all-features のみ）。
#
# ★ ゲートを足す人への方針（機械の守りと同じ場所に置く）:
#   ゲートを足したら、その feature に依存するテストは同じ cfg で囲み、「なぜこの構成では
#   意味が無いか」を 1 行書く（定義が feature 依存なら期待値も同じ cfg で組む）。囲み忘れは
#   R6 が下で bare のテストを走らせて赤くする。
#
# 範囲の判断: 「外した構成でテストを回す経路が 1 つも無い」状態を無くすのが目的なので、
# **全ゲート off（バレ）1 本**に絞る。単一 feature（nostr/web/discord のみ）や全部入りまで
# テストを回すと、compile 単位が 4〜5 通り増えて CI が重くなる割に、feature 依存の取りこぼしは
# 「全部 off」で最もよく露出する（定義・ルート・発火経路がまるごと消えるため）。全部入りは既定の
# CI テストが既に覆う。単一 feature 構成のテストは今回ローカルで緑を確認済み（必要なら別途 issue で
# マトリクス拡張）。追加コストは opencrab-server の no-default テストバイナリのビルド＋実行（実測
# 約 10 秒台の実行 + そのビルド）で、既存の R6 no-default ビルドと成果物を概ね共有する。
#
# `-p opencrab-server` を必ず付ける: ワークスペース root で --no-default-features を付けても
# feature 統合で server の既定は落ちない（段階 1 の E2E で判明済み）。
cargo build -p opencrab-gateway
cargo test -p opencrab-server --no-default-features
for f in discord nostr web; do
  cargo build -p opencrab-server --no-default-features --features "$f"
done
cargo build -p opencrab-server
echo "R6(full matrix + no-default tests) OK"
