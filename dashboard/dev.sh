#!/usr/bin/env bash
# Dashboard dev server: dx serve (WASM client) + server binary (API)
# Usage: ./dev.sh [start|stop|restart|status|logs]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DX_PID_FILE="$SCRIPT_DIR/.dev-server.pid"
SERVER_PID_FILE="$SCRIPT_DIR/.server.pid"
DX_LOG_FILE="$SCRIPT_DIR/.dev-server.log"
SERVER_LOG_FILE="$SCRIPT_DIR/.server.log"
DX_PORT=8081       # dx serve devserver port (WASM client + hot reload)
SERVER_PORT=3000    # Server binary port (API endpoints)

export OPENCRAB_DB="$PROJECT_ROOT/data/opencrab.db"

_is_running() {
    [ -f "$1" ] && kill -0 "$(cat "$1")" 2>/dev/null
}

# Recursively kill a process and all its children
_kill_tree() {
    local pid="$1"
    kill -0 "$pid" 2>/dev/null || return 0
    local children
    children=$(pgrep -P "$pid" 2>/dev/null || true)
    for child in $children; do
        _kill_tree "$child"
    done
    kill "$pid" 2>/dev/null || true
}

_stop_one() {
    local pf="$1" label="$2"
    if _is_running "$pf"; then
        local pid
        pid=$(cat "$pf")
        echo "Stopping $label (PID $pid)..."
        _kill_tree "$pid"
        for _ in $(seq 1 30); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null || true
    fi
    rm -f "$pf"
}

_stop() {
    _stop_one "$DX_PID_FILE" "dx serve"
    _stop_one "$SERVER_PID_FILE" "server binary"
    # Clean orphans on both ports
    for port in $DX_PORT $SERVER_PORT; do
        local orphan
        orphan=$(lsof -ti:"$port" 2>/dev/null || true)
        if [ -n "$orphan" ]; then
            echo "Cleaning orphan(s) on port $port: $orphan"
            echo "$orphan" | xargs kill 2>/dev/null || true
        fi
    done
    echo "Stopped."
}

_start() {
    if _is_running "$DX_PID_FILE"; then
        echo "Already running. Use 'restart'."
        return 1
    fi

    for port in $DX_PORT $SERVER_PORT; do
        local port_user
        port_user=$(lsof -ti:"$port" 2>/dev/null || true)
        if [ -n "$port_user" ]; then
            echo "Error: Port $port in use by PID(s): $port_user"
            return 1
        fi
    done

    cd "$SCRIPT_DIR"

    # 1) Start server binary (API endpoints) on fixed port
    echo "Starting server binary (port $SERVER_PORT)..."
    PORT=$SERVER_PORT cargo run --features server > "$SERVER_LOG_FILE" 2>&1 &
    echo $! > "$SERVER_PID_FILE"

    # 2) Start dx serve (WASM client + Tailwind + hot reload)
    echo "Starting dx serve (port $DX_PORT) with hot reload..."
    dx serve --port "$DX_PORT" --open false > "$DX_LOG_FILE" 2>&1 &
    echo $! > "$DX_PID_FILE"

    sleep 10
    local ok=true
    if ! _is_running "$DX_PID_FILE"; then
        echo "Error: dx serve failed to start:"
        tail -10 "$DX_LOG_FILE"
        ok=false
    fi

    echo ""
    echo "Dashboard dev environment:"
    echo "  Browser:  http://localhost:$DX_PORT"
    echo "  Server:   http://localhost:$SERVER_PORT"
    echo "  DB:       $OPENCRAB_DB"
    echo "  Logs:     $DX_LOG_FILE (dx), $SERVER_LOG_FILE (server)"
    if [ "$ok" != "true" ]; then
        return 1
    fi
}

_status() {
    echo "dx serve:  $(_is_running "$DX_PID_FILE" && echo "running (PID $(cat "$DX_PID_FILE"))" || echo "stopped")"
    echo "server:    $(_is_running "$SERVER_PID_FILE" && echo "running (PID $(cat "$SERVER_PID_FILE"))" || echo "stopped")"
    for port in $DX_PORT $SERVER_PORT; do
        local port_user
        port_user=$(lsof -ti:"$port" 2>/dev/null || true)
        echo "Port $port:  ${port_user:+in use by PID(s) $port_user}${port_user:-free}"
    done
}

_logs() {
    echo "=== dx serve ==="
    [ -f "$DX_LOG_FILE" ] && tail -15 "$DX_LOG_FILE" || echo "No log."
    echo ""
    echo "=== server ==="
    [ -f "$SERVER_LOG_FILE" ] && tail -15 "$SERVER_LOG_FILE" || echo "No log."
}

case "${1:-start}" in
    start)   _start ;;
    stop)    _stop ;;
    restart) _stop; sleep 1; _start ;;
    status)  _status ;;
    logs)    _logs ;;
    *) echo "Usage: $0 {start|stop|restart|status|logs}"; exit 1 ;;
esac
