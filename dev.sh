#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

SERVER_PID_FILE="$SCRIPT_DIR/.server.pid"
WEB_PID_FILE="$SCRIPT_DIR/.web.pid"
FEATURES="discord"

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
}

start_web() {
    stop_web 2>/dev/null || true
    echo "==> Starting frontend dev server..."
    (cd "$SCRIPT_DIR/web" && npx vite --port 3000) > "$SCRIPT_DIR/.web.log" 2>&1 &
    local pid=$!
    echo "$pid" > "$WEB_PID_FILE"
    echo "    Frontend started (PID: $pid) → http://localhost:3000"
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
        echo "==> Ready: http://localhost:3000"
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
