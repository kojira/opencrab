#!/usr/bin/env bash
# 公開リポジトリに既知の非公開識別子が再混入するのを止める。
#
# 2026-08-22: レビューを何巡も回しながら一度も指摘されず、278 箇所が公開状態だった
# （テストの固定値・コメント・UI のプレースホルダ）。人が見る手順では止まらないので
# 機械で落とす。
#
# この検査が見るもの:
#   - 既知の運用者ハンドル、個人名、稼働エージェント名と表記ゆれ
#   - 既知の Discord チャンネル名、外部システム名、場所を示す実例
#   - 今回発見した Discord snowflake、agent/session/skill ID、外部 URL
#   - 平文メールアドレス（example.com / example.test / .invalid の合成値は除外）
#
# この検査が見ないもの:
#   - 未知の名前・ID・URL（実在値と合成 fixture を構造だけで安全に区別できない）
#   - Git 履歴・commit metadata・untracked file・生成物・符号化/分割された文字列
#   - 公開サービスの API/ドキュメント URL や、別プロジェクトの CLI 名（nostaro 等）
#
# したがってこれは「今回判明した既知値の denylist」であって、完全な個人情報検出器ではない。
# 新しい種類は人のレビューで確認し、値と表記ゆれをここへ追加する。
set -euo pipefail
cd "$(dirname "$0")/.."

# 自分自身は denylist を保持するため走査対象から外す。
GREP_SCOPE=(-- . ':!scripts/check-no-private-identifiers.sh')

# 稼働中個体・外部システム・実運用の場・場所の既知表記。1 行 1 値。
KNOWN_TEXT=(
  'らぼみ'
  'かいろ'
  'のすたろう'
  'rabomi'
  'kairo'
  'nostarou'
  'scout'
  'sebastian'
  'omoikane'
  'omoiakne'
  'もぐたろう'
  'kairo-test'
  'らぼみ実験室'
  'Agent Hub'
  'Hakata'
  '博多'
  'github.com/kojira/nostaro'
  'owner.io'
)

# 実運用から採取された既知 ID。合成 fixture の snowflake / UUID は対象にしない。
KNOWN_IDS=(
  '1157167346817958009'
  '1465697209541726362'
  '1470698801395273861'
  '1479115942293409942'
  '54fab4ec-fad2-45d9-92dd-e62e50e2b36b'
  '6b79ac3a-7f17-4618-a827-5bda992a3698'
  'eeb24f06-2f66-4816-a3bd-fbd558b50dee'
  'fdb8d880'
  '24428fff'
)

fail=0
check_fixed() {
  local kind=$1
  local value=$2
  local hits
  hits="$(git grep -inIF "$value" "${GREP_SCOPE[@]}" 2>/dev/null || true)"
  if [ -n "$hits" ]; then
    echo "[error] ${kind} '$value' が追跡ファイルに入っています:"
    printf '%s\n' "$hits" | head -20
    fail=1
  fi
}

for value in "${KNOWN_TEXT[@]}"; do
  check_fixed '既知の識別文字列' "$value"
done
for value in "${KNOWN_IDS[@]}"; do
  check_fixed '既知の実 ID' "$value"
done

# 運用者ハンドルだけは正当な参照にも必要なので、許可文字列を除去した残りを見る。
# 許可するのは (1) この repository 自身の URL/issue shorthand と、(2) 挙動維持に必要な
# 稼働中の既定 Nostr relay の exact hostname だけ。行単位では除外しない。
operator_hits="$(git grep -inIF 'kojira' "${GREP_SCOPE[@]}" 2>/dev/null || true)"
if [ -n "$operator_hits" ]; then
  unexpected_operator_hits="$(printf '%s\n' "$operator_hits" \
    | sed -E 's|https://github\.com/kojira/opencrab[^[:space:]<>"]*||g; s|kojira/opencrab#[0-9]+||g; s|r\.kojira\.io||g' \
    | grep -i 'kojira' || true)"
  if [ -n "$unexpected_operator_hits" ]; then
    echo "[error] 許可参照以外の運用者ハンドルが追跡ファイルに入っています:"
    printf '%s\n' "$unexpected_operator_hits" | head -20
    fail=1
  fi
fi

# 平文メール。公開用の予約ドメインだけは合成 fixture として許可する。
email_hits="$(git grep -nIE '[[:alnum:]._%+-]+@[[:alnum:].-]+\.[[:alpha:]]{2,}' "${GREP_SCOPE[@]}" 2>/dev/null \
  | grep -vEi '@([^[:space:]]+\.)?(example\.com|example\.test|invalid)([^[:alnum:]]|$)' || true)"
if [ -n "$email_hits" ]; then
  echo "[error] 平文メールアドレスが追跡ファイルに入っています:"
  printf '%s\n' "$email_hits" | head -20
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  cat <<'MSG'

テストの固定値・コメント・UI 文言に実在値を入れないでください。
値の意味が読める中立名か、明らかに合成と分かる fixture を使ってください。
MSG
  exit 1
fi
echo "既知の非公開識別子の混入なし"
