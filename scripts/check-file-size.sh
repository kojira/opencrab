#!/usr/bin/env bash
# ソースファイルの行数 ratchet ゲート（#937）。
# オーナー指示「ソースファイルは 800 行以内」を CI で機械執行する。
#
# 対象: crates/**/*.rs のうち、テストを除いた production ソース。
#   除外: tests/ 配下・*_tests.rs・tests.rs（今回はテストを対象外にする）。
#
# 判定（ratchet）:
#   - allowlist に無いファイルが 800 行を超えたら赤。
#     （新規ファイル・既存 800 行以下ファイルの肥大化を止める）
#   - allowlist にあるファイルが記録行数より増えたら赤。
#     （既存 800 行超ファイルはこれ以上大きくできない）
#   - allowlist にあるファイルが記録行数より減ったら警告（赤にはしない）。
#     `--update` を付けて allowlist を減った値へ書き換えると、その値で凍結される。
#     一度減らした行数は戻せない（減った分は戻れない ratchet）。
#
# 使い方:
#   scripts/check-file-size.sh            判定のみ（CI で使う。違反があれば exit 1）
#   scripts/check-file-size.sh --update   allowlist を現在行数へ ratchet down して書き換える
#   scripts/check-file-size.sh --generate allowlist を現在の 800 行超ファイルで作り直す
#
# bash 3.2（macOS 既定）でも動くよう連想配列は使わない。依存は coreutils のみ。

set -euo pipefail
cd "$(dirname "$0")/.."

LIMIT=800
ALLOWLIST="scripts/file-size-allowlist.txt"
MODE="check"

case "${1:-}" in
  --update)   MODE="update" ;;
  --generate) MODE="generate" ;;
  "")         MODE="check" ;;
  *) echo "usage: $0 [--update|--generate]" >&2; exit 2 ;;
esac

# 対象ファイル一覧（テストを除いた production ソース）を安定順で列挙する。
list_target_files() {
  find crates -name '*.rs' -type f \
    | grep -v '/tests/' \
    | grep -v '_tests\.rs$' \
    | grep -v '/tests\.rs$' \
    | LC_ALL=C sort
}

# allowlist から path の記録行数を引く（無ければ空文字）。
recorded_lines() {
  local path=$1
  [ -f "$ALLOWLIST" ] || return 0
  awk -F'\t' -v p="$path" '$2==p{print $1; exit}' "$ALLOWLIST"
}

# --generate: 現在 800 行超のファイルを現在行数で凍結した allowlist を書き出す。
if [ "$MODE" = "generate" ]; then
  tmp=$(mktemp)
  list_target_files | while IFS= read -r f; do
    n=$(wc -l < "$f" | tr -d ' ')
    if [ "$n" -gt "$LIMIT" ]; then
      printf '%s\t%s\n' "$n" "$f"
    fi
  done > "$tmp"
  mv "$tmp" "$ALLOWLIST"
  echo "generated $ALLOWLIST: $(wc -l < "$ALLOWLIST" | tr -d ' ') files frozen (> ${LIMIT} lines)"
  exit 0
fi

red=0        # 赤（違反）件数
warn=0       # 警告（減少・要 update）件数
red_rows=""  # 赤の一覧
warn_rows=""  # 警告の一覧
update_tmp=""
[ "$MODE" = "update" ] && update_tmp=$(mktemp)

while IFS= read -r f; do
  n=$(wc -l < "$f" | tr -d ' ')
  rec=$(recorded_lines "$f")

  if [ -n "$rec" ]; then
    # allowlist にあるファイル。
    if [ "$n" -gt "$rec" ]; then
      red=$((red + 1))
      red_rows="${red_rows}$(printf '  %6s  (記録 %6s, +%s)  %s' "$n" "$rec" "$((n - rec))" "$f")
"
      [ "$MODE" = "update" ] && printf '%s\t%s\n' "$rec" "$f" >> "$update_tmp"
    elif [ "$n" -lt "$rec" ]; then
      warn=$((warn + 1))
      warn_rows="${warn_rows}$(printf '  %6s  (記録 %6s, -%s)  %s' "$n" "$rec" "$((rec - n))" "$f")
"
      # 減少は ratchet down: update ならその値で凍結し直す。
      [ "$MODE" = "update" ] && printf '%s\t%s\n' "$n" "$f" >> "$update_tmp"
    else
      [ "$MODE" = "update" ] && printf '%s\t%s\n' "$rec" "$f" >> "$update_tmp"
    fi
  else
    # allowlist に無いファイル。
    if [ "$n" -gt "$LIMIT" ]; then
      red=$((red + 1))
      red_rows="${red_rows}$(printf '  %6s  (上限 %6s, +%s)  %s' "$n" "$LIMIT" "$((n - LIMIT))" "$f")
"
    fi
  fi
done <<EOF
$(list_target_files)
EOF

# allowlist にあるが対象一覧に無い（削除・改名済み）エントリを検出する。
stale=0
stale_rows=""
if [ -f "$ALLOWLIST" ]; then
  targets=$(list_target_files)
  while IFS="$(printf '\t')" read -r rec_n rec_path; do
    [ -z "${rec_path:-}" ] && continue
    if ! printf '%s\n' "$targets" | grep -qxF "$rec_path"; then
      stale=$((stale + 1))
      stale_rows="${stale_rows}  (記録 ${rec_n})  ${rec_path}
"
    fi
  done < "$ALLOWLIST"
fi

# --update: 書き換えた allowlist を確定する（stale エントリは落とす）。
if [ "$MODE" = "update" ]; then
  LC_ALL=C sort -t"$(printf '\t')" -k2 "$update_tmp" > "$ALLOWLIST"
  rm -f "$update_tmp"
  echo "updated $ALLOWLIST: $(wc -l < "$ALLOWLIST" | tr -d ' ') files"
  [ "$stale" -gt 0 ] && echo "dropped $stale stale entr(y/ies)"
  exit 0
fi

# ---- 判定結果の出力 ----
if [ "$red" -gt 0 ]; then
  echo "赤: 行数上限違反 ${red} 件（行数 / 記録・上限 / パス）:"
  printf '%s' "$red_rows"
fi
if [ "$warn" -gt 0 ]; then
  echo "警告: 記録より減少 ${warn} 件。scripts/check-file-size.sh --update で allowlist を更新せよ:"
  printf '%s' "$warn_rows"
fi
if [ "$stale" -gt 0 ]; then
  echo "警告: allowlist に削除/改名済みエントリ ${stale} 件。--update で除去せよ:"
  printf '%s' "$stale_rows"
fi

if [ "$red" -gt 0 ]; then
  echo "FAIL: ソースファイル行数ゲート（上限 ${LIMIT} 行）に違反があります。"
  exit 1
fi

echo "OK: ソースファイル行数ゲート（上限 ${LIMIT} 行, allowlist $( [ -f "$ALLOWLIST" ] && wc -l < "$ALLOWLIST" | tr -d ' ' || echo 0 ) 件）を満たしています。"
exit 0
