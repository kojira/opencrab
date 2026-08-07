#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

SERVER_PID_FILE="$SCRIPT_DIR/.server.pid"
WEB_PID_FILE="$SCRIPT_DIR/.web.pid"
FEATURES="discord"

# #417: 実サーバプロセスを PID ファイルに頼らず探すためのパターン。
# start_server は ./target/debug/opencrab-server を起動するので、その実行ファイルパスで照合する。
SERVER_PROC_PATTERN="target/debug/opencrab-server"

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
    ./target/debug/opencrab-server > "$SCRIPT_DIR/.server.log" 2>&1 &
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
