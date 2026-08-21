#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
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

# #712: サーバが後始末コード（kill_on_drop）を走らせられずに死ぬ経路
# （kill -9・panic・OOM・電源断・graceful shutdown を持たない旧バイナリ）で、
# nostaro watch の子プロセスが ppid=1 の孤児として残る。次の起動をまたいで watcher が
# 多重化するのを防ぐため、サーバ本体を止めた直後・新しい子を spawn する前のこの入口で消す。
#
# 規律（#417 の絶対パス照合を、nostaro 子の --config 絶対パスに読み替えたもの）:
#  - パターン kill 禁止。PID を列挙 → 個別に kill する。pkill -f nostaro は新しい子や別
#    checkout の隔離テスト用 nostaro まで巻き込む。
#  - 照合は「バイナリ名 nostaro（argv0 の basename）」と「--config が $SCRIPT_DIR/data/agents/
#    で始まる」の両方を要求する。nostaro は PATH 上の共有バイナリ（cli.rs の
#    DEFAULT_NOSTARO_PATH="nostaro"）で checkout を区別できないため、区別の鍵は --config 接頭辞。
#  - kill する前に ppid==1（孤児）であることを確認する。ppid!=1 の一致は「殺すべき親付き
#    プロセス」＝想定外なので kill せずログに残す。
#  - degrade（plan_cwd_and_config が None を返し --config が相対で渡る）で取りこぼした
#    nostaro 孤児は黙って見逃さず警告する。殺さないのは pattern-kill 禁止のため、黙らないのは
#    no-fallback のため。
stop_orphan_nostaro() {
    local config_prefix="$SCRIPT_DIR/data/agents/"
    local killed_count=0
    local orphan_total=0
    local pid ppid rest exe base config_matches

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
                echo "==> 孤児 nostaro watcher を停止 (PID: $pid)"
                kill "$pid" 2>/dev/null || true
                killed_count=$((killed_count + 1))
            else
                # この checkout の子なのに親が生きている。stop_server 直後にこれが出るのは
                # 想定外（本体を殺し損ねている可能性）。kill せず記録する。
                echo "==> [warn] この checkout の nostaro が親付きで稼働中 (PID: $pid, PPID: $ppid)。kill しません。"
            fi
        fi
    done < <(ps -axww -o pid=,ppid=,command=)

    # degrade で取りこぼした孤児 nostaro（ppid=1 だが --config が接頭辞に一致せず checkout を
    # 確証できないもの）を黙って見逃さない。別 checkout の正当な孤児もここに数え上がり得るが、
    # 「確証できないため kill しない」という観測結果として fail-loud に出す。
    local unattributed=$((orphan_total - killed_count))
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
