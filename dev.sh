#!/usr/bin/env bash
set -euo pipefail

# pwd -P で物理パス（symlink 解決済み）にする。#712 の孤児 nostaro 掃除は、この
# SCRIPT_DIR を接頭辞に nostaro 子の --config を照合する。--config はサーバの
# std::env::current_dir()（getcwd＝物理パス）由来なので、こちらが論理パス（pwd、symlink を
# 残す）だと経路に symlink が 1 つでもあると文字列が食い違い、自 checkout の正当な孤児を
# 取りこぼす。SERVER_BIN と stop_stray_servers の pgrep 照合、server の cw（下の cd）も
# すべてこの SCRIPT_DIR 由来なので、物理パスに揃えて齟齬を無くす（server の cwd も下の cd 由来）。
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
cd "$SCRIPT_DIR"

SERVER_PID_FILE="$SCRIPT_DIR/.server.pid"
WEB_PID_FILE="$SCRIPT_DIR/.web.pid"
FEATURES="discord"

# #417: サーバ実行ファイルの絶対パス。start_server の argv と stop_stray_servers の
# pgrep 照合の両方でこれを使う。
#
# 相対部分文字列（"target/debug/opencrab-server"）だと checkout を区別できず、別 worktree の
# 隔離テスト用サーバや、そのパスを引数に持つ無関係プロセスまで巻き込んで kill してしまう。
# このチェックアウト配下の絶対パスで一致させ、「この checkout のサーバだけ」に絞る。
SERVER_BIN="$SCRIPT_DIR/target/debug/opencrab-server"
SERVER_PROC_PATTERN="$SERVER_BIN"

usage() {
    cat <<EOF
Usage: ./dev.sh <command>

Commands:
  start     Build and start backend + frontend
  stop      Stop backend + frontend
  restart   Rebuild and restart backend (frontend stays)
  server    Build and start backend only
  web       Start frontend dev server only
  status    Show running processes
  logs      Tail server log
EOF
}

build_server() {
    echo "==> Building server (features: $FEATURES)..."
    cargo build --features "$FEATURES"
}

start_server() {
    stop_server 2>/dev/null || true
    build_server
    echo "==> Starting server..."
    # 絶対パスで起動する。argv が SERVER_PROC_PATTERN と一致し、stop_stray_servers が
    # この checkout のサーバだけを検出できる（#417）。
    "$SERVER_BIN" > "$SCRIPT_DIR/.server.log" 2>&1 &
    local pid=$!
    echo "$pid" > "$SERVER_PID_FILE"
    echo "    Server started (PID: $pid)"
}

# #417: PID ファイルに載っていない実サーバプロセスを止める。
#
# PID ファイルが stale（別 run の古い PID／OS が別プロセスへ再利用）だと、stop_server は
# 実サーバを止められないまま start_server が新サーバを上げ、二重起動になる（同一 SQLite への
# 二重ライタ・Discord の二重処理）。PID ファイルとは別に実プロセスを pgrep で探し、生きて
# いれば止めて単一インスタンスを保証する。
stop_stray_servers() {
    local pids
    pids=$(pgrep -f "$SERVER_PROC_PATTERN" 2>/dev/null || true)
    if [ -z "$pids" ]; then
        return 0
    fi
    echo "==> PID ファイル外で稼働中の opencrab-server を検出 (PID: $(echo "$pids" | tr '\n' ' '))"
    echo "    二重起動を避けるため停止します..."
    # shellcheck disable=SC2086
    kill $pids 2>/dev/null || true
    # Wait up to 5s for graceful shutdown
    for _ in $(seq 1 50); do
        pgrep -f "$SERVER_PROC_PATTERN" >/dev/null 2>&1 || break
        sleep 0.1
    done
    # Force kill if still running
    if pgrep -f "$SERVER_PROC_PATTERN" >/dev/null 2>&1; then
        pkill -9 -f "$SERVER_PROC_PATTERN" 2>/dev/null || true
    fi
    echo "    停止しました"
}

# PID を 1 つ、死ぬまで確実に終わらせる。TERM → 最大 5 秒ポーリング → 残存なら KILL(-9)
# → 再度ポーリングして消滅を確認する。「送った」ではなく「消えた」を戻り値で表す:
#   0 = 消滅を確認 / 1 = TERM の送信に失敗（権限・PID 再利用・既に消滅）/ 2 = -9 後も残存
# stop_stray_servers は同じ規律（kill → 最大 5 秒待機 → 残存なら -9）をパターン一括で持つ。
# こちらは孤児掃除のため PID 個別に適用するが、待ち方・エスカレーション・死亡確認の規律を
# 揃える（3 つ目の子プロセス種別が来たら共通化を検討する余地がある）。
terminate_pid() {
    local pid="$1"
    local _
    # 握り潰さない: TERM が送れなければ理由を呼び出し側へ返す（rc=1）。
    if ! kill "$pid" 2>/dev/null; then
        return 1
    fi
    for _ in $(seq 1 50); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.1
    done
    # TERM で消えない → -9 へエスカレーション。
    kill -9 "$pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.1
    done
    return 2
}

# #712: サーバが後始末コード（kill_on_drop）を走らせられずに死ぬ経路
# （kill -9・panic・OOM・電源断・graceful shutdown を持たない旧バイナリ）で、
# nostaro watch の子プロセスが ppid=1 の孤児として残る。次の起動をまたいで watcher が
# 多重化するのを防ぐため、サーバ本体を止めた直後・新しい子を spawn する前のこの入口で消す。
#
# 規律（#417 の絶対パス照合を、nostaro 子の --config 絶対パスに読み替えたもの）:
#  - パターン kill 禁止。PID を列挙 → 個別に kill する。pkill -f nostaro は新しい子や別
#    checkout の隔離テスト用 nostaro まで巻き込む。
#  - 照合は「バイナリ名 nostaro（argv0 の basename）」と「--config が $SCRIPT_DIR/data/agents/
#    で始まる」の両方を要求する。nostaro のバイナリは opencrab の checkout の外（別プロジェクト
#    の絶対パス、実測では /Volumes/2TB/openclaw/workspace/projects/nostaro/target/release/nostaro）
#    にあり、この opencrab checkout を指さない。よってバイナリのパスでは checkout を区別できず、
#    区別の鍵はこの checkout 配下を指す --config 接頭辞になる。
#  - kill する前に ppid==1（孤児）であることを確認する。ppid!=1 の一致は「殺すべき親付き
#    プロセス」＝想定外なので kill せずログに残す。これは孤児が pid 1 に付き直す darwin/launchd
#    前提の環境結合。プロセススーパーバイザや subreaper 配下では孤児が別の ppid に付くため、
#    自 checkout の孤児でも config_matches=1 && ppid!=1 の「親付き」warn に落ち kill されず
#    蓄積する（warn は出るので黙りはしない）。その環境では ppid 判定の見直しが要る。
#  - degrade（plan_cwd_and_config が None を返し --config が相対で渡る）で取りこぼした
#    nostaro 孤児は黙って見逃さず警告する。殺さないのは pattern-kill 禁止のため、黙らないのは
#    no-fallback のため。
stop_orphan_nostaro() {
    local config_prefix="$SCRIPT_DIR/data/agents/"
    # matched_orphans = ppid==1 かつ config 一致（この checkout に帰属できた孤児）。
    # kill が成功したかではなく「帰属できたか」で数える。kill 失敗は個別に fail-loud で
    # 出すので、帰属できた孤児は下の unattributed から差し引く（過小計上を避ける）。
    local matched_orphans=0
    local orphan_total=0
    local pid ppid rest exe base config_matches rc

    # ps -axww: argv を切り詰めずに全取得。先頭 2 語が pid・ppid、残りが command 全体。
    while read -r pid ppid rest; do
        [ -z "${rest:-}" ] && continue
        exe=${rest%% *}       # argv0（フルパス or "nostaro"）
        base=${exe##*/}       # basename
        [ "$base" = "nostaro" ] || continue

        # --config が $SCRIPT_DIR/data/agents/ で始まる = この checkout の子。
        case "$rest" in
            *"--config ${config_prefix}"*) config_matches=1 ;;
            *)                             config_matches=0 ;;
        esac

        if [ "$ppid" = "1" ]; then
            orphan_total=$((orphan_total + 1))
        fi

        if [ "$config_matches" = "1" ]; then
            if [ "$ppid" = "1" ]; then
                # 帰属できた孤児として数える（kill 成否に依らず）。
                matched_orphans=$((matched_orphans + 1))
                echo "==> 孤児 nostaro watcher を停止します (PID: $pid)..."
                # 「送った」ではなく「消えた」を確認してから成功ログを出す。
                if terminate_pid "$pid"; then
                    echo "    停止を確認しました (PID: $pid)"
                else
                    rc=$?
                    if [ "$rc" = "1" ]; then
                        # TERM が送れなかった。握り潰さず理由を添えて warn（他の PID は続行）。
                        echo "==> [warn] 孤児 nostaro の kill に失敗 (PID: $pid)。権限・PID 再利用・既に消滅の可能性。スキップして継続します。"
                    else
                        # -9 でも消えない。狙った不変条件（掃除できた）が破れているので error。
                        echo "==> [error] 孤児 nostaro が SIGKILL 後も残存 (PID: $pid)。手動確認が必要です。" >&2
                    fi
                fi
            else
                # この checkout の子なのに親が生きている。stop_server 直後にこれが出るのは
                # 想定外（本体を殺し損ねている可能性）。kill せず記録する。
                echo "==> [warn] この checkout の nostaro が親付きで稼働中 (PID: $pid, PPID: $ppid)。kill しません。"
            fi
        fi
    done < <(ps -axww -o pid=,ppid=,command=)

    # 帰属できなかった孤児 nostaro（ppid=1 だが --config が接頭辞に一致しないもの）を黙って
    # 見逃さない。このカウンタは 2 つの原因を混ぜており、切り分けられない（仕様として許容）:
    #   (a) 自 checkout の degrade 孤児（plan_cwd_and_config が None → --config が相対で渡る）
    #   (b) 別 checkout が同じ共有マシンに残した正当な孤児
    # (b) が居ると、こちらの restart のたびに無関係な孤児についても警告が出る。それでも
    # 「帰属できず kill しない」という観測結果を fail-loud に出す（no-fallback）。
    # matched_orphans（kill 成否に依らず帰属できた数）を引くので、kill 失敗でこの値が
    # 過小になって警告が黙ることはない（kill 失敗は上で個別に warn 済み）。
    local unattributed=$((orphan_total - matched_orphans))
    if [ "$unattributed" -gt 0 ]; then
        echo "==> [warn] ppid=1 の孤児 nostaro を ${unattributed} 個検出しましたが、--config が"
        echo "    ${config_prefix} で始まらず checkout を確証できないため kill しません。"
        echo "    （--config が相対で渡る degrade か、別 checkout の孤児の可能性）"
    fi
}

stop_server() {
    if [ -f "$SERVER_PID_FILE" ]; then
        local pid
        pid=$(cat "$SERVER_PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            echo "==> Stopping server (PID: $pid)..."
            kill "$pid"
            # Wait up to 5s for graceful shutdown
            for _ in $(seq 1 50); do
                kill -0 "$pid" 2>/dev/null || break
                sleep 0.1
            done
            # Force kill if still running
            if kill -0 "$pid" 2>/dev/null; then
                kill -9 "$pid" 2>/dev/null || true
            fi
            echo "    Server stopped"
        else
            echo "    Server not running (stale PID file)"
        fi
        rm -f "$SERVER_PID_FILE"
    else
        echo "    No server PID file found"
    fi

    # #417: PID ファイル経由の停止だけでは、stale/欠落時に実プロセスが残る。
    # 起動前に必ず実プロセスを掃除して二重起動を防ぐ（残っていなければ no-op）。
    stop_stray_servers

    # #712: サーバ本体を落とした直後に、親を失った nostaro watcher の孤児を掃除する。
    # ここは stop / restart / start（start_server 冒頭）すべてが通る入口で、新しい子を
    # spawn する build より前なので誤爆の窓が無い。
    stop_orphan_nostaro
}

start_web() {
    stop_web 2>/dev/null || true
    echo "==> Starting frontend dev server..."
    # --host 0.0.0.0 は必須。省くと vite はループバック（[::1]）だけで listen し、
    # 127.0.0.1 からも LAN 上の別端末（スマホ等）からもダッシュボードに届かなくなる。
    (cd "$SCRIPT_DIR/web" && npx vite --port 3000 --host 0.0.0.0) > "$SCRIPT_DIR/.web.log" 2>&1 &
    local pid=$!
    echo "$pid" > "$WEB_PID_FILE"
    echo "    Frontend started (PID: $pid) → http://localhost:3000"
    echo "    別端末からは .web.log の \"Network:\" 行の URL を使う"
}

stop_web() {
    if [ -f "$WEB_PID_FILE" ]; then
        local pid
        pid=$(cat "$WEB_PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            echo "==> Stopping frontend (PID: $pid)..."
            kill "$pid" 2>/dev/null || true
            echo "    Frontend stopped"
        else
            echo "    Frontend not running (stale PID file)"
        fi
        rm -f "$WEB_PID_FILE"
    else
        echo "    No frontend PID file found"
    fi
}

show_status() {
    echo "==> Status"
    if [ -f "$SERVER_PID_FILE" ]; then
        local pid
        pid=$(cat "$SERVER_PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            echo "    Server:   running (PID: $pid)"
        else
            echo "    Server:   not running (stale PID)"
        fi
    else
        echo "    Server:   not running"
    fi

    if [ -f "$WEB_PID_FILE" ]; then
        local pid
        pid=$(cat "$WEB_PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            echo "    Frontend: running (PID: $pid)"
        else
            echo "    Frontend: not running (stale PID)"
        fi
    else
        echo "    Frontend: not running"
    fi
}

case "${1:-}" in
    start)
        start_server
        start_web
        echo ""
        echo "==> Ready: http://localhost:3000 (別端末からは .web.log の Network URL)"
        ;;
    stop)
        stop_server
        stop_web
        ;;
    restart)
        start_server
        echo ""
        echo "==> Server restarted"
        ;;
    server)
        start_server
        ;;
    web)
        start_web
        ;;
    status)
        show_status
        ;;
    logs)
        tail -f "$SCRIPT_DIR/.server.log"
        ;;
    *)
        usage
        exit 1
        ;;
esac
