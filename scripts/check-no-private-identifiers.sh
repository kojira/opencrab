#!/usr/bin/env bash
# 公開リポジトリに、構造から判定できる非公開識別子が混入するのを止める。
# 構造だけでは判定できない名前や個別値は、追跡外の .private-identifiers から読む。
#
# この検査が見るもの:
#   - 17〜20 桁の Discord snowflake（反復・連番の合成 fixture は除外）
#   - 32 byte の Nostr 鍵に相当する npub1 / nsec1（標準・合成 fixture は除外）
#   - 平文メールアドレス（予約ドメインの fixture は除外）
#   - 外部ホストを指す wss:// endpoint（公共 relay・予約ドメイン等は除外）
#   - .private-identifiers がある場合は、その 1 行 1 値の固定文字列
#
# この検査が見ないもの:
#   - .private-identifiers がない環境での名前・UUID・短い ID などの個別値
#   - 合成 fixture と同じ反復・連番構造の数値
#   - Nostr の hex 鍵、npub/nsec 以外の NIP-19 識別子、壊れた bech32 文字列
#   - http:// / https://、ローカル・単一ラベル・テンプレートの wss:// endpoint
#   - Git 履歴、commit metadata、untracked file、生成物、符号化・分割された文字列
#
# これは完全な個人情報検出器ではない。特に、追跡外一覧がない CI では名前を検査しない。
# 負のテスト（検査が実際に落ちるか試す）で使う値について:
#   実在のホスト名・鍵・ID を使わない。外部サービスは閉鎖・移転する（実例: 2026-08 時点で
#   relay.damus.io は閉鎖済み）。実在値を使うと、テストの前提が現実とずれていく。
#   予約ドメイン（example.com / example.test / .invalid、RFC 2606/6761）と、
#   明らかに合成と分かる形（反復・連番）を使うこと。
#   ただし「明らかに合成」の値はこの検査が意図的に見逃すので、
#   構造検査の負のテストには「実在しないが実在の形をした」値を作って使う。

# 実行する前に `git add` を済ませること。この検査は追跡ファイルだけを見るので、
# 未追跡のまま実行すると新規ファイルが対象外になり、CI で初めて落ちる（実際に起きた）。

set -euo pipefail
cd "$(dirname "$0")/.."

GREP_SCOPE=(-- .)
PRIVATE_IDENTIFIERS=.private-identifiers
fail=0

report_fixed_hits() {
  local kind=$1
  local value=$2
  local hits
  hits="$(git grep -nIF -e "$value" "${GREP_SCOPE[@]}" 2>/dev/null || true)"
  if [ -n "$hits" ]; then
    echo "[error] ${kind} が追跡ファイルに入っています:"
    printf '%s\n' "$hits" | head -20
    fail=1
  fi
}

# このリポジトリ自身の GitHub URL・issue 参照・CI badge は公開情報であり、リンクを
# 壊さないため許可する。owner 名をスクリプトへ複製せず、origin から exact prefix を得る。
SELF_GITHUB_PREFIX="$(git remote get-url origin 2>/dev/null \
  | sed -nE 's#^(git@github\.com:|https://github\.com/)([^/]+/opencrab)(\.git)?$#https://github.com/\2#p' \
  | head -1 || true)"
if [ -z "$SELF_GITHUB_PREFIX" ]; then
  SELF_GITHUB_PREFIX="$(git grep -hoIE 'https://github\.com/[^/[:space:]]+/opencrab' -- README.md 2>/dev/null \
    | head -1 || true)"
fi

# 既定 relay は製品が接続する公共 Nostr relay で、個人の endpoint ではないため許可する。
# first label が r の既定 relay は Damus の日本語圏の既定で、誰でも接続できる公共 server。
# 同じ domain にある first label が x の relay も公共 relay として許可対象である。
PUBLIC_RELAY_HOSTS=()
while IFS= read -r host; do
  [ -n "$host" ] || continue
  PUBLIC_RELAY_HOSTS+=("$host")
  case "$host" in
    r.*) PUBLIC_RELAY_HOSTS+=("x.${host#*.}") ;;
  esac
done < <(sed -nE '/^pub const DEFAULT_RELAYS:/p' crates/nostr/src/config.rs 2>/dev/null \
  | grep -oE 'wss://[[:alnum:].-]+' \
    | sed 's|^wss://||' | sort -u)

is_public_relay_host() {
  local candidate=$1
  local allowed
  for allowed in "${PUBLIC_RELAY_HOSTS[@]}"; do
    [ "$candidate" = "$allowed" ] && return 0
  done
  return 1
}

# 反復単位、循環する昇順・降順、同じ長さの数字群による連番は、テストで使うことが
# 明白な合成 snowflake として許可する。これと同じ形の値は検査できない。
is_synthetic_snowflake() {
  local value=$1
  local length=${#value}
  local period unit rebuilt i tail digit previous run compressed all_runs_repeated
  local ascending=01234567890123456789012345678901234567890
  local descending=09876543210987654321098765432109876543210

  period=1
  while [ "$period" -le 10 ]; do
    if [ $((length % period)) -eq 0 ]; then
      unit=${value:0:period}
      rebuilt=""
      i=0
      while [ "$i" -lt $((length / period)) ]; do
        rebuilt="${rebuilt}${unit}"
        i=$((i + 1))
      done
      [ "$rebuilt" = "$value" ] && return 0
    fi
    period=$((period + 1))
  done

  i=0
  while [ "$i" -le 3 ]; do
    tail=${value:i}
    if [ "${#tail}" -ge 15 ]; then
      case "$ascending" in *"$tail"*) return 0 ;; esac
      case "$descending" in *"$tail"*) return 0 ;; esac
    fi
    i=$((i + 1))
  done

  compressed=""
  previous=""
  run=0
  all_runs_repeated=1
  i=0
  while [ "$i" -lt "$length" ]; do
    digit=${value:i:1}
    if [ "$digit" = "$previous" ]; then
      run=$((run + 1))
    else
      [ -z "$previous" ] || [ "$run" -ge 2 ] || all_runs_repeated=0
      compressed="${compressed}${digit}"
      previous=$digit
      run=1
    fi
    i=$((i + 1))
  done
  [ "$run" -ge 2 ] || all_runs_repeated=0
  if [ "$all_runs_repeated" -eq 1 ] && [ "${#compressed}" -ge 4 ]; then
    case "$ascending" in *"$compressed"*) return 0 ;; esac
    case "$descending" in *"$compressed"*) return 0 ;; esac
  fi
  return 1
}

while IFS= read -r candidate; do
  [ -n "$candidate" ] || continue
  is_synthetic_snowflake "$candidate" && continue
  report_fixed_hits 'Discord snowflake' "$candidate"
done < <(git grep -ohIE '(^|[^0-9])[0-9]{17,20}([^0-9]|$)' "${GREP_SCOPE[@]}" 2>/dev/null \
  | tr -cd '0-9\n' | sort -u || true)

is_allowed_nostr_key() {
  case "$1" in
    # NIP-19 本文の公開 example。相互運用テストに必要な固定ベクタなので許可する。
    npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg) return 0 ;;
    nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5) return 0 ;;
    # NIP-19 の正準ベクトル（hex 3bf0c63f... とのペア）。規格が公開している検証用の値で
    # 誰の鍵でもない。bech32 の実装が仕様どおりかを釘付けにするために必要。
    npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6) return 0 ;;
    # 32 byte がすべて 0 の決定的な合成 fixture。実鍵としては無効なので許可する。
    nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqzqujme) return 0 ;;
  esac
  return 1
}

while IFS= read -r candidate; do
  [ -n "$candidate" ] || continue
  is_allowed_nostr_key "$candidate" && continue
  report_fixed_hits 'Nostr の npub/nsec 鍵' "$candidate"
done < <(git grep -ohIE '(npub1|nsec1)[023456789acdefghjklmnpqrstuvwxyz]{58}' \
  "${GREP_SCOPE[@]}" 2>/dev/null | sort -u || true)

# 平文メールは local part と domain の構造で拾う。RFC 2606 / RFC 6761 の予約 domain
# （example.com/net/org、.example、.test、.invalid、localhost）は合成 fixture なので許可する。
while IFS= read -r addr; do
  [ -n "$addr" ] || continue
  domain=${addr##*@}
  case "$domain" in
    example.com|*.example.com|example.net|*.example.net|example.org|*.example.org|\
    example|*.example|test|*.test|invalid|*.invalid|localhost|*.localhost) continue ;;
  esac
  report_fixed_hits '平文メールアドレス' "$addr"
done < <(git grep -ohIE '[[:alnum:]._%+-]+@[[:alnum:].-]+\.[[:alpha:]]{2,}' \
  "${GREP_SCOPE[@]}" 2>/dev/null | sort -u || true)

# wss:// の host だけを見る。公共 relay、予約 domain、loopback、単一ラベル、明示的な
# 合成二番目 relay は実 endpoint ではないため除外する。http(s) と path は検査しない。
while IFS= read -r endpoint; do
  [ -n "$endpoint" ] || continue
  host=${endpoint#wss://}
  host=${host%%:*}
  is_public_relay_host "$host" && continue
  case "$host" in
    localhost|*.localhost|127.*|0.0.0.0|\
    example|*.example|example.com|*.example.com|example.net|*.example.net|\
    example.org|*.example.org|test|*.test|invalid|*.invalid|relay.two) continue ;;
    *.*) ;;
    *) continue ;;
  esac
  report_fixed_hits '許可されていない外部 wss endpoint' "$endpoint"
done < <(git grep -ohIE 'wss://[[:alnum:]][[:alnum:].-]*(:[0-9]{1,5})?' \
  "${GREP_SCOPE[@]}" 2>/dev/null | sort -u || true)

strip_fixed() {
  local text=$1
  local needle=$2
  local before after
  [ -n "$needle" ] || { printf '%s' "$text"; return; }
  while case "$text" in *"$needle"*) true ;; *) false ;; esac; do
    before=${text%%"$needle"*}
    after=${text#*"$needle"}
    text=${before}${after}
  done
  printf '%s' "$text"
}

strip_allowed_references() {
  local text=$1
  local allowed host
  if [ -n "$SELF_GITHUB_PREFIX" ]; then
    # URL 全体を消す必要はなく、名前を含む self repository prefix を消せば判定できる。
    text=$(strip_fixed "$text" "$SELF_GITHUB_PREFIX")
    allowed=${SELF_GITHUB_PREFIX#https://github.com/}
    text=$(strip_fixed "$text" "$allowed")
  fi
  for host in "${PUBLIC_RELAY_HOSTS[@]}"; do
    text=$(strip_fixed "$text" "wss://${host}")
    text=$(strip_fixed "$text" "$host")
  done
  printf '%s' "$text"
}

# 名前・場所・個体名・外部システム名・UUID・短い ID・個別 URL などは構造だけでは
# 実在性を判定できない。必要な環境だけが、追跡外ファイルへ 1 行 1 値で保持する。
if [ -f "$PRIVATE_IDENTIFIERS" ]; then
  echo "[notice] ${PRIVATE_IDENTIFIERS} を使って名前・個別値を検査します。"
  entry_number=0
  while IFS= read -r value || [ -n "$value" ]; do
    entry_number=$((entry_number + 1))
    value=${value%$'\r'}
    [ -n "$value" ] || continue
    hits="$(git grep -inIF -e "$value" "${GREP_SCOPE[@]}" 2>/dev/null || true)"
    unexpected_hits=""
    while IFS= read -r hit; do
      [ -n "$hit" ] || continue
      remainder=$(strip_allowed_references "$hit")
      case "$remainder" in
        *"$value"*) unexpected_hits="${unexpected_hits}${hit}"$'\n' ;;
      esac
    done <<< "$hits"
    if [ -n "$unexpected_hits" ]; then
      echo "[error] ${PRIVATE_IDENTIFIERS} の ${entry_number} 行目にある識別子が追跡ファイルに入っています:"
      printf '%s' "$unexpected_hits" | head -20
      fail=1
    fi
  done < "$PRIVATE_IDENTIFIERS"
else
  echo "[notice] ${PRIVATE_IDENTIFIERS} がないため、名前・個別値の検査は行っていません。構造検査は続行します。"
fi

if [ "$fail" -ne 0 ]; then
  cat <<'MSG'

テストの固定値・コメント・UI 文言に実在値を入れないでください。
値の意味が読める中立名か、明らかに合成と分かる fixture を使ってください。
MSG
  exit 1
fi
echo "検査対象の非公開識別子の混入なし"
