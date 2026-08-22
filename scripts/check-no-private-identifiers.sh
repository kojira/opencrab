#!/usr/bin/env bash
# 公開リポジトリに運用者・稼働中エージェントの実名が入るのを止める。
#
# 2026-08-22: レビューを何巡も回しながら一度も指摘されず、278 箇所が公開状態だった
# （テストの固定値・コメント・UI のプレースホルダ）。人が見る手順では止まらないので
# 機械で落とす。
#
# ここは「既知の実名」だけを見る網であって、封じ込めではない。
# 新しい種類（新しい運用者名・新しい個体名）は人のレビューで拾い、ここへ足す。
set -euo pipefail
cd "$(dirname "$0")/.."

# 稼働中の個体名・運用者ハンドル。追加するときは 1 行 1 語。
PATTERNS=(
  'kojira'
  'らぼみ'
  'かいろ'
  'のすたろう'
)

fail=0
for p in "${PATTERNS[@]}"; do
  # このリポジトリ自身の GitHub URL は正当（消すとリンクが壊れる）。
  hits="$(git grep -inI "$p" -- . ':!scripts/check-no-private-identifiers.sh' 2>/dev/null \
          | grep -v 'github\.com/kojira' || true)"
  if [ -n "$hits" ]; then
    echo "[error] 実名 '$p' が追跡ファイルに入っています（公開リポジトリ）:"
    echo "$hits" | head -20
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  cat <<'MSG'

テストの固定値・コメント・UI 文言に実名を入れないでください。
テストは自分でフィクスチャを作るか、明らかに合成と分かる値を使ってください。
MSG
  exit 1
fi
echo "個人識別子の混入なし"
