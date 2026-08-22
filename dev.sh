#!/usr/bin/env bash
set -euo pipefail

# New-runtime development launcher.
#
# A long-lived supervisor owns one process group per component.  Every process
# in a component inherits an open descriptor for that component's owner file.
# Stopping a verified group, instead of only its leader, also stops any
# cursor/shell child that the core may have started during a turn.  The owner
# descriptor remains usable after either the leader or supervisor dies, so the
# next `stop` can safely clean the group without trusting a reusable PID alone.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
cd "$SCRIPT_DIR"

die() {
    echo "[error] $*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Usage: ./dev.sh <command>

Commands:
  start             Build and start core + configured gates
  stop              Stop the supervisor, gates, core, and their descendants
  restart           Build first, then stop and start everything
  status             Show the supervisor and every configured component
  logs [component]   Follow core, web, nostr, or launcher log (default: core)

Start configuration:
  OPENCRAB_DEV_DIR          State directory (default: .opencrab-dev)
  OPENCRAB_DEV_HTTP_PORT    web-gate port (default: 3000)
  OPENCRAB_DEV_WEB_TOKEN    web-gate token (default: secret-token)
  OPENCRAB_DEV_ROOM         default core room (default: room:main)
  OPENCRAB_DEV_GATES        web, nostr, or web,nostr (default: web)

Nostr is never started implicitly.  When OPENCRAB_DEV_GATES contains nostr,
NOSTR_GATE_RELAY must also be set explicitly.  Runtime variables such as
OPENCRAB_PLACES and OPENCRAB_LLM_PROVIDER are passed through unchanged.
EOF
}

init_paths() {
    local requested="${OPENCRAB_DEV_DIR:-$SCRIPT_DIR/.opencrab-dev}"
    case "$requested" in
        /*) ;;
        *) requested="$SCRIPT_DIR/$requested" ;;
    esac
    mkdir -p "$requested"
    DEV_DIR="$(cd "$requested" && pwd -P)"
    PID_DIR="$DEV_DIR/pids"
    LOG_DIR="$DEV_DIR/logs"
    CORE_SOCKET="$DEV_DIR/core.sock"
    DB_PATH="$DEV_DIR/opencrab.db"
    SETTINGS_FILE="$DEV_DIR/settings"
    SUPERVISOR_PID_FILE="$PID_DIR/supervisor.pid"
    SUPERVISOR_LOCK="$DEV_DIR/supervisor.lock"
    LIFECYCLE_LOCK="$DEV_DIR/lifecycle.lock"
    READY_FILE="$DEV_DIR/ready"
    FAILED_FILE="$DEV_DIR/failed"
    mkdir -p "$PID_DIR" "$LOG_DIR"

    local requested_target="${CARGO_TARGET_DIR:-$SCRIPT_DIR/target}"
    case "$requested_target" in
        /*) ;;
        *) requested_target="$SCRIPT_DIR/$requested_target" ;;
    esac
    mkdir -p "$requested_target"
    TARGET_DIR="$(cd "$requested_target" && pwd -P)"
    CORE_BIN="$TARGET_DIR/debug/opencrab-social-runtime"
    WEB_BIN="$TARGET_DIR/debug/web-gate"
    NOSTR_BIN="$TARGET_DIR/debug/nostr-gate"
    LOCK_BIN="$TARGET_DIR/debug/opencrab-lock-fd"
}

validate_port() {
    case "$1" in
        ''|*[!0-9]*) return 1 ;;
    esac
    [ "$1" -ge 1 ] && [ "$1" -le 65535 ]
}

gate_enabled() {
    case ",$GATES," in
        *,"$1",*) return 0 ;;
        *) return 1 ;;
    esac
}

load_start_config() {
    HTTP_PORT="${OPENCRAB_DEV_HTTP_PORT:-3000}"
    WEB_TOKEN="${OPENCRAB_DEV_WEB_TOKEN:-secret-token}"
    ROOM="${OPENCRAB_DEV_ROOM:-room:main}"
    GATES="${OPENCRAB_DEV_GATES:-web}"

    validate_port "$HTTP_PORT" || die "OPENCRAB_DEV_HTTP_PORT must be an integer from 1 to 65535"
    [ -n "$ROOM" ] || die "OPENCRAB_DEV_ROOM must not be empty"
    case "$GATES" in
        ''|,*|*,|*,,*) die "OPENCRAB_DEV_GATES must be a non-empty comma-separated list" ;;
    esac

    local old_ifs="$IFS"
    local gate
    local seen_web=0
    local seen_nostr=0
    IFS=,
    for gate in $GATES; do
        case "$gate" in
            web)
                [ "$seen_web" -eq 0 ] || die "OPENCRAB_DEV_GATES contains web more than once"
                seen_web=1
                ;;
            nostr)
                [ "$seen_nostr" -eq 0 ] || die "OPENCRAB_DEV_GATES contains nostr more than once"
                seen_nostr=1
                ;;
            '') die "OPENCRAB_DEV_GATES contains an empty gate name" ;;
            *) die "unknown gate in OPENCRAB_DEV_GATES: $gate" ;;
        esac
    done
    IFS="$old_ifs"
    [ "$seen_web" -eq 1 ] || [ "$seen_nostr" -eq 1 ] || die "OPENCRAB_DEV_GATES must select at least one gate"
    if [ "$seen_web" -eq 1 ]; then
        [ -n "$WEB_TOKEN" ] || die "OPENCRAB_DEV_WEB_TOKEN must not be empty"
        command -v curl >/dev/null 2>&1 || die "curl is required to verify web-gate readiness"
    fi

    if [ "$seen_nostr" -eq 1 ]; then
        [ -n "${NOSTR_GATE_RELAY:-}" ] || die "NOSTR_GATE_RELAY is required when nostr is selected"
        [ -n "${OPENCRAB_PLACES:-}" ] || die "OPENCRAB_PLACES is required to provision a place when nostr is selected"
    fi
    command -v cargo >/dev/null 2>&1 || die "cargo is required to build the runtime"
    command -v nohup >/dev/null 2>&1 || die "nohup is required to detach the launcher supervisor"
    if [ ! -d /proc/self/fd ] && ! command -v lsof >/dev/null 2>&1; then
        die "lsof is required on systems without /proc to verify PID ownership safely"
    fi

}

read_pid() {
    local file="$1"
    local pid
    [ -f "$file" ] || return 1
    pid="$(sed -n '1p' "$file")"
    case "$pid" in
        ''|*[!0-9]*) return 2 ;;
    esac
    printf '%s\n' "$pid"
}

pid_alive() {
    kill -0 "$1" 2>/dev/null
}

group_alive() {
    kill -0 -- "-$1" 2>/dev/null
}

# Confirm that a process still holds the owner file inherited at component
# spawn.  A process with a reused PID, even one running the same executable,
# cannot acquire this descriptor retroactively.
process_holds_component_owner() {
    local name="$1"
    local pid="$2"
    local owner_file="$PID_DIR/$name.owner"
    [ -f "$owner_file" ] || return 2

    if [ -d "/proc/$pid/fd" ]; then
        local fd target
        for fd in /proc/"$pid"/fd/*; do
            target="$(readlink "$fd" 2>/dev/null || true)"
            [ "$target" = "$owner_file" ] && return 0
        done
        return 1
    fi
    command -v lsof >/dev/null 2>&1 || return 2
    lsof -Fn -a -p "$pid" -- "$owner_file" 2>/dev/null | grep -Fqx "n$owner_file"
}

# Verify ownership from any live member, not only the process-group leader.
# This is what lets cleanup reclaim children after the leader exits first.
component_owned() {
    local name="$1"
    local pgid="$2"
    local owner_file="$PID_DIR/$name.owner"
    local stat line rest state parent member_pgid member rc
    local unverifiable=0
    [ -f "$owner_file" ] || return 2

    if [ -d /proc/self/fd ]; then
        for stat in /proc/[0-9]*/stat; do
            [ -r "$stat" ] || continue
            line="$(sed -n '1p' "$stat" 2>/dev/null || true)"
            [ -n "$line" ] || continue
            member="${line%% *}"
            # comm is parenthesized and may contain spaces or ')'.  Removing
            # through the final ') ' leaves: state ppid pgrp ...
            rest="${line##*) }"
            read -r state parent member_pgid rest <<< "$rest"
            [ "$member_pgid" = "$pgid" ] || continue
            if process_holds_component_owner "$name" "$member"; then
                return 0
            else
                rc=$?
                [ "$rc" -eq 2 ] && unverifiable=1
            fi
        done
        [ "$unverifiable" -eq 0 ] || return 2
        return 1
    fi

    command -v lsof >/dev/null 2>&1 || return 2
    if lsof -a -g "$pgid" -Fn -- "$owner_file" 2>/dev/null | grep -Fqx "n$owner_file"; then
        return 0
    fi
    [ "$unverifiable" -eq 0 ] || return 2
    return 1
}

supervisor_owned() {
    local pid="$1"
    if [ -d "/proc/$pid/fd" ]; then
        local fd target
        for fd in /proc/"$pid"/fd/*; do
            target="$(readlink "$fd" 2>/dev/null || true)"
            [ "$target" = "$SUPERVISOR_LOCK" ] && return 0
        done
        return 1
    fi
    command -v lsof >/dev/null 2>&1 || return 2
    lsof -Fn -a -p "$pid" -- "$SUPERVISOR_LOCK" 2>/dev/null | grep -Fqx "n$SUPERVISOR_LOCK"
}

acquire_lifecycle_lock() {
    if [ ! -x "$LOCK_BIN" ] && [ -f "$SETTINGS_FILE" ]; then
        local saved_target
        saved_target="$(sed -n 's/^target_dir=//p' "$SETTINGS_FILE" | sed -n '1p')"
        if [ -n "$saved_target" ] && [ -x "$saved_target/debug/opencrab-lock-fd" ]; then
            LOCK_BIN="$saved_target/debug/opencrab-lock-fd"
        fi
    fi
    [ -x "$LOCK_BIN" ] || die "launcher lock helper is missing; run './dev.sh start' to build it"

    # launch_supervisor() duplicates FD 9 into the detached supervisor as FD 7.
    # Both descriptors then refer to the same open-file description, so either
    # process can keep the lifecycle lock held if the other exits first.
    exec 9> "$LIFECYCLE_LOCK"
    "$LOCK_BIN" 9 --wait || die "could not acquire the launcher lifecycle lock"
}

run_with_lifecycle_lock() {
    local rc=0
    acquire_lifecycle_lock
    "$@" || rc=$?
    exec 9>&-
    return "$rc"
}

build_components() {
    local cargo_args=(
        build --locked
        -p opencrab-app --bin opencrab-social-runtime
        -p opencrab-app --bin opencrab-lock-fd
    )
    if gate_enabled web; then
        cargo_args+=( -p opencrab-web-gate --bin web-gate )
    fi
    if gate_enabled nostr; then
        cargo_args+=( -p opencrab-nostr-gate --bin nostr-gate )
    fi
    echo "==> Building core and gates: $GATES"
    CARGO_TARGET_DIR="$TARGET_DIR" cargo "${cargo_args[@]}"
}

write_settings() {
    local places="default ($ROOM)"
    local provider="echo"
    [ -z "${OPENCRAB_PLACES:-}" ] || places="$OPENCRAB_PLACES"
    [ -z "${OPENCRAB_LLM_PROVIDER:-}" ] || provider="$OPENCRAB_LLM_PROVIDER"
    {
        echo "state_dir=$DEV_DIR"
        echo "socket=$CORE_SOCKET"
        echo "database=$DB_PATH"
        echo "gates=$GATES"
        if gate_enabled web; then
            echo "web_url=http://127.0.0.1:$HTTP_PORT"
        fi
        echo "places=$places"
        echo "llm_provider=$provider"
        if gate_enabled nostr; then
            echo "nostr_relay=$NOSTR_GATE_RELAY"
        fi
        echo "target_dir=$TARGET_DIR"
    } > "$SETTINGS_FILE"
}

state_has_live_processes() {
    local file pid name
    if pid="$(read_pid "$SUPERVISOR_PID_FILE" 2>/dev/null)" && pid_alive "$pid"; then
        return 0
    fi
    for name in core web nostr; do
        file="$PID_DIR/$name.pid"
        if pid="$(read_pid "$file" 2>/dev/null)" && group_alive "$pid"; then
            return 0
        fi
    done
    return 1
}

clean_dead_state() {
    local name
    rm -f "$SUPERVISOR_PID_FILE" "$READY_FILE" "$FAILED_FILE" "$CORE_SOCKET"
    for name in core web nostr; do
        rm -f "$PID_DIR/$name.pid" "$PID_DIR/$name.owner"
    done
}

start_component() {
    local name="$1"
    shift
    local owner_file="$PID_DIR/$name.owner"
    : > "$LOG_DIR/$name.log"
    : > "$owner_file"
    (
        # Only the supervisor owns the startup lifecycle descriptor.  If a
        # component retained it, stop/restart could wait forever after a
        # supervisor failure.
        exec 7>&-
        # FD 8 is intentionally inherited by the component and its children.
        exec 8> "$owner_file"
        exec "$@"
    ) >> "$LOG_DIR/$name.log" 2>&1 &
    local pid=$!
    printf '%s\n' "$pid" > "$PID_DIR/$name.pid"
    if ! group_alive "$pid"; then
        echo "[error] $name did not start in its own process group" >&2
        return 1
    fi
    echo "==> Started $name (PID/PGID: $pid)"
}

wait_for_socket() {
    local pid="$1"
    local i
    for ((i = 0; i < 100; i++)); do
        [ -S "$CORE_SOCKET" ] && return 0
        pid_alive "$pid" || return 1
        sleep 0.1
    done
    return 1
}

wait_for_web() {
    local pid="$1"
    local url="http://127.0.0.1:$HTTP_PORT/__opencrab_launcher_ready"
    local response
    local i
    for ((i = 0; i < 100; i++)); do
        response="$(curl --noproxy '*' --silent --show-error --fail --max-time 1 "$url" 2>/dev/null || true)"
        if [ "$response" = "$WEB_READY_TOKEN" ] && pid_alive "$pid" \
            && component_owned web "$pid"; then
            # With a custom places file the launcher does not know which web
            # room to probe.  Otherwise also wait for the configured room to
            # be bound to core, as the previous readiness check did.
            if [ -n "${OPENCRAB_PLACES:-}" ]; then
                return 0
            fi
            case "$ROOM" in
                room:*)
                    local room_name="${ROOM#room:}"
                    if curl --noproxy '*' --silent --show-error --fail --max-time 1 \
                        "http://127.0.0.1:$HTTP_PORT/rooms/$room_name/messages" >/dev/null 2>&1; then
                        return 0
                    fi
                    ;;
                *) return 0 ;;
            esac
        fi
        pid_alive "$pid" || return 1
        sleep 0.1
    done
    return 1
}

wait_for_nostr() {
    local pid="$1"
    local expected="nostr-gate: relay 接続 $NOSTR_GATE_RELAY"
    local i
    for ((i = 0; i < 150; i++)); do
        grep -Fq "$expected" "$LOG_DIR/nostr.log" 2>/dev/null && return 0
        pid_alive "$pid" || return 1
        sleep 0.1
    done
    return 1
}

terminate_component() {
    local name="$1"
    local pid_file="$PID_DIR/$name.pid"
    local pid rc i
    if pid="$(read_pid "$pid_file" 2>/dev/null)"; then
        :
    else
        rc=$?
        if [ "$rc" -eq 2 ]; then
            echo "[error] Invalid PID file: $pid_file" >&2
            return 1
        fi
        return 0
    fi

    terminate_component_pid "$name" "$pid"
}

terminate_component_pid() {
    local name="$1"
    local pid="$2"
    local pid_file="$PID_DIR/$name.pid"
    local rc i

    if ! group_alive "$pid"; then
        remove_component_state_if_matches "$name" "$pid" || return 1
        return 0
    fi

    if component_owned "$name" "$pid"; then
        :
    else
        rc=$?
        if [ "$rc" -eq 2 ]; then
            echo "[error] Cannot verify ownership of $name process group $pid; refusing to signal it" >&2
        else
            echo "[error] $name process group $pid does not hold this launcher's owner file; refusing to signal it" >&2
        fi
        return 1
    fi

    echo "==> Stopping $name (PGID: $pid)"
    kill -TERM -- "-$pid" 2>/dev/null || true
    for ((i = 0; i < 50; i++)); do
        group_alive "$pid" || break
        sleep 0.1
    done
    if group_alive "$pid"; then
        echo "    $name did not stop after 5s; sending SIGKILL"
        # TERM may have removed the original group and freed its numeric PGID.
        # Re-check the inherited owner FD before signaling that number again.
        if component_owned "$name" "$pid"; then
            kill -KILL -- "-$pid" 2>/dev/null || true
        else
            echo "[error] Ownership of $name process group $pid changed; refusing SIGKILL" >&2
            return 1
        fi
        for ((i = 0; i < 20; i++)); do
            group_alive "$pid" || break
            sleep 0.1
        done
    fi
    if group_alive "$pid"; then
        echo "[error] $name process group $pid remains after SIGKILL" >&2
        return 1
    fi
    remove_component_state_if_matches "$name" "$pid" || return 1
    echo "    $name stopped"
}

remove_component_state_if_matches() {
    local name="$1"
    local expected_pid="$2"
    local pid_file="$PID_DIR/$name.pid"
    local registered_pid
    if registered_pid="$(read_pid "$pid_file" 2>/dev/null)" \
        && [ "$registered_pid" != "$expected_pid" ]; then
        echo "[error] $name PID registration changed; preserving the newer state" >&2
        return 1
    fi
    rm -f "$pid_file" "$PID_DIR/$name.owner"
}

supervisor_cleanup() {
    local rc=$?
    trap - EXIT TERM INT HUP
    local failed=0
    terminate_component nostr || failed=1
    terminate_component web || failed=1
    terminate_component core || failed=1
    rm -f "$READY_FILE" "$CORE_SOCKET" "$SUPERVISOR_PID_FILE"
    # Failure/termination cleanup is now complete.  This is the earliest point
    # at which a waiting lifecycle command may inspect and mutate state.
    exec 7>&-
    if [ "$failed" -ne 0 ]; then
        echo "[error] Supervisor could not stop every component" >&2
        exit 1
    fi
    exit "$rc"
}

supervise() {
    : "${OC_CORE_BIN:?}" "${OC_WEB_BIN:?}" "${OC_NOSTR_BIN:?}" "${OC_LOCK_BIN:?}"
    : "${OC_HTTP_PORT:?}" "${OC_WEB_TOKEN:?}" "${OC_WEB_READY_TOKEN:?}" "${OC_ROOM:?}" "${OC_GATES:?}"
    CORE_BIN="$OC_CORE_BIN"
    WEB_BIN="$OC_WEB_BIN"
    NOSTR_BIN="$OC_NOSTR_BIN"
    LOCK_BIN="$OC_LOCK_BIN"
    HTTP_PORT="$OC_HTTP_PORT"
    WEB_TOKEN="$OC_WEB_TOKEN"
    WEB_READY_TOKEN="$OC_WEB_READY_TOKEN"
    ROOM="$OC_ROOM"
    GATES="$OC_GATES"
    TARGET_DIR="$OC_TARGET_DIR"
    NOSTR_GATE_RELAY="${OC_NOSTR_GATE_RELAY:-}"

    # FD 7 is the startup lifecycle lock inherited from launch_supervisor().
    # Keep it separate from the long-lived supervisor lock on FD 9 until ready
    # publication or failure cleanup has completed.
    if ! : >&7 2>/dev/null; then
        die "launcher supervisor is missing its inherited lifecycle lock"
    fi

    # flock(2) attaches to the shared open-file description, so this lock
    # remains held after the helper exits and for the supervisor's lifetime.
    # Nothing below may read or mutate shared launcher state before it succeeds.
    exec 9> "$SUPERVISOR_LOCK"
    "$LOCK_BIN" 9 || die "another launcher start is already in progress or running"

    state_has_live_processes && die "launcher state still has live processes; run './dev.sh stop' first"
    clean_dead_state
    write_settings
    : > "$LOG_DIR/launcher.log"
    printf '%s\n' "$$" > "$SUPERVISOR_PID_FILE"
    rm -f "$READY_FILE" "$FAILED_FILE" "$CORE_SOCKET"
    trap 'exit 0' TERM INT HUP
    trap supervisor_cleanup EXIT

    # Monitor mode gives every background component its own process group.
    set -m
    start_component core "$CORE_BIN" "$CORE_SOCKET" "$DB_PATH" "$ROOM" || {
        echo "core failed to start" > "$FAILED_FILE"
        exit 1
    }
    local core_pid
    core_pid="$(read_pid "$PID_DIR/core.pid")"
    wait_for_socket "$core_pid" || {
        echo "core did not create its Unix socket within 10s" > "$FAILED_FILE"
        exit 1
    }

    if gate_enabled web; then
        start_component web env WEB_GATE_BIND=127.0.0.1 \
            OPENCRAB_WEB_READY_TOKEN="$WEB_READY_TOKEN" \
            "$WEB_BIN" "$CORE_SOCKET" "$HTTP_PORT" "$WEB_TOKEN" || {
            echo "web gate failed to start" > "$FAILED_FILE"
            exit 1
        }
        local web_pid
        web_pid="$(read_pid "$PID_DIR/web.pid")"
        wait_for_web "$web_pid" || {
            echo "web gate was not ready within 10s" > "$FAILED_FILE"
            exit 1
        }
    fi

    if gate_enabled nostr; then
        start_component nostr env NOSTR_GATE_RELAY="$NOSTR_GATE_RELAY" \
            "$NOSTR_BIN" "$CORE_SOCKET" || {
            echo "nostr gate failed to start" > "$FAILED_FILE"
            exit 1
        }
        local nostr_pid
        nostr_pid="$(read_pid "$PID_DIR/nostr.pid")"
        wait_for_nostr "$nostr_pid" || {
            echo "nostr gate did not connect to the explicit relay within 15s" > "$FAILED_FILE"
            exit 1
        }
    fi

    touch "$READY_FILE"
    echo "==> All configured components are ready"
    # All state for this generation is now published.  The foreground command
    # still has FD 9 when it is alive; if it died, closing FD 7 releases the
    # shared lifecycle lock and lets a waiting stop/restart proceed.
    exec 7>&-

    local name pid
    while :; do
        for name in core web nostr; do
            [ -f "$PID_DIR/$name.pid" ] || continue
            pid="$(read_pid "$PID_DIR/$name.pid")" || {
                echo "invalid $name PID file" > "$FAILED_FILE"
                exit 1
            }
            if ! pid_alive "$pid"; then
                echo "$name exited unexpectedly; see $LOG_DIR/$name.log" > "$FAILED_FILE"
                exit 1
            fi
        done
        sleep 0.5
    done
}

show_failure_logs() {
    local name
    for name in core web nostr launcher; do
        [ -s "$LOG_DIR/$name.log" ] || continue
        echo "--- $name.log (last 20 lines) ---" >&2
        tail -n 20 "$LOG_DIR/$name.log" >&2
    done
}

launch_supervisor() {
    local web_ready_token
    web_ready_token="$(LC_ALL=C od -An -N16 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n')"
    [ "${#web_ready_token}" -eq 32 ] || die "could not generate a web-gate readiness token"

    export OC_CORE_BIN="$CORE_BIN"
    export OC_WEB_BIN="$WEB_BIN"
    export OC_NOSTR_BIN="$NOSTR_BIN"
    export OC_LOCK_BIN="$LOCK_BIN"
    export OC_HTTP_PORT="$HTTP_PORT"
    export OC_WEB_TOKEN="$WEB_TOKEN"
    export OC_WEB_READY_TOKEN="$web_ready_token"
    export OC_ROOM="$ROOM"
    export OC_GATES="$GATES"
    export OC_TARGET_DIR="$TARGET_DIR"
    export OC_NOSTR_GATE_RELAY="${NOSTR_GATE_RELAY:-}"

    # dup(2) semantics make child FD 7 share FD 9's open-file description and
    # therefore its lifecycle lock.  Only the detached child receives FD 7.
    nohup bash "$SCRIPT_DIR/dev.sh" __supervise 7>&9 </dev/null >> "$LOG_DIR/launcher.log" 2>&1 &
    local supervisor_pid=$!
    local registered_pid
    local owned_state=0
    local i
    for ((i = 0; i < 180; i++)); do
        if registered_pid="$(read_pid "$SUPERVISOR_PID_FILE" 2>/dev/null)" \
            && [ "$registered_pid" = "$supervisor_pid" ] \
            && pid_alive "$supervisor_pid" \
            && supervisor_owned "$supervisor_pid"; then
            owned_state=1
            if [ -f "$FAILED_FILE" ]; then
                echo "[error] $(cat "$FAILED_FILE")" >&2
                wait "$supervisor_pid" 2>/dev/null || true
                show_failure_logs
                return 1
            fi
            if [ -f "$READY_FILE" ]; then
                echo "==> Ready"
                cat "$SETTINGS_FILE"
                echo "logs=$LOG_DIR"
                return 0
            fi
        fi
        if ! pid_alive "$supervisor_pid"; then
            wait "$supervisor_pid" 2>/dev/null || true
            if [ "$owned_state" -eq 1 ]; then
                echo "[error] launcher supervisor exited before readiness" >&2
                show_failure_logs
            else
                echo "[error] launcher start lost the exclusive lock; another start is in progress or running" >&2
            fi
            return 1
        fi
        sleep 0.1
    done
    echo "[error] launcher supervisor did not become ready within 18s" >&2
    show_failure_logs
    if registered_pid="$(read_pid "$SUPERVISOR_PID_FILE" 2>/dev/null)" \
        && [ "$registered_pid" = "$supervisor_pid" ] \
        && supervisor_owned "$supervisor_pid"; then
        kill -TERM "$supervisor_pid" 2>/dev/null || true
        wait "$supervisor_pid" 2>/dev/null || true
    else
        echo "[error] timed-out supervisor ownership changed; refusing to signal it" >&2
    fi
    return 1
}

stop_all() {
    local failed=0
    local supervisor_pid rc i name pid
    local core_pid="" web_pid="" nostr_pid=""

    # Freeze the recovery targets before signaling the supervisor.  The
    # lifecycle lock prevents another command from publishing a new generation,
    # and these snapshots ensure recovery never selects a target by rereading a
    # PID file after the old supervisor has exited.
    for name in core web nostr; do
        if pid="$(read_pid "$PID_DIR/$name.pid" 2>/dev/null)"; then
            case "$name" in
                core) core_pid="$pid" ;;
                web) web_pid="$pid" ;;
                nostr) nostr_pid="$pid" ;;
            esac
        else
            rc=$?
            if [ "$rc" -eq 2 ]; then
                echo "[error] Invalid PID file: $PID_DIR/$name.pid" >&2
                failed=1
            fi
        fi
    done

    if supervisor_pid="$(read_pid "$SUPERVISOR_PID_FILE" 2>/dev/null)"; then
        if pid_alive "$supervisor_pid"; then
            if supervisor_owned "$supervisor_pid"; then
                echo "==> Stopping launcher supervisor (PID: $supervisor_pid)"
                kill -TERM "$supervisor_pid" 2>/dev/null || failed=1
                for ((i = 0; i < 120; i++)); do
                    pid_alive "$supervisor_pid" || break
                    sleep 0.1
                done
                if pid_alive "$supervisor_pid"; then
                    echo "    supervisor did not stop after 12s; sending SIGKILL"
                    if supervisor_owned "$supervisor_pid"; then
                        kill -KILL "$supervisor_pid" 2>/dev/null || failed=1
                    else
                        echo "[error] Supervisor ownership changed; refusing SIGKILL" >&2
                        failed=1
                    fi
                fi
            else
                rc=$?
                if [ "$rc" -eq 2 ]; then
                    echo "[error] Cannot verify supervisor PID $supervisor_pid; refusing to signal it" >&2
                else
                    echo "[error] Supervisor PID $supervisor_pid does not hold this launcher's lock" >&2
                fi
                failed=1
            fi
        else
            echo "==> Supervisor is not running; cleaning its registered process groups"
        fi
    elif [ -f "$SUPERVISOR_PID_FILE" ]; then
        echo "[error] Invalid supervisor PID file: $SUPERVISOR_PID_FILE" >&2
        failed=1
    fi

    # Normally the supervisor already removed these.  This is the recovery path
    # after SIGKILL/panic and is what prevents registered orphan groups building up.
    [ -z "$nostr_pid" ] || terminate_component_pid nostr "$nostr_pid" || failed=1
    [ -z "$web_pid" ] || terminate_component_pid web "$web_pid" || failed=1
    [ -z "$core_pid" ] || terminate_component_pid core "$core_pid" || failed=1

    if [ "$failed" -ne 0 ]; then
        echo "[error] Not every launcher process was stopped" >&2
        return 1
    fi
    clean_dead_state
    echo "==> Stopped; no registered process group remains"
}

restart_all() {
    stop_all || return 1
    launch_supervisor
}

component_status() {
    local name="$1"
    local pid rc
    if pid="$(read_pid "$PID_DIR/$name.pid" 2>/dev/null)"; then
        :
    else
        if [ -f "$PID_DIR/$name.pid" ]; then
            echo "    $name: invalid PID file"
            return 1
        fi
        echo "    $name: not configured/running"
        return 0
    fi
    if pid_alive "$pid"; then
        if component_owned "$name" "$pid"; then
            echo "    $name: running (PID/PGID $pid, log $LOG_DIR/$name.log)"
            return 0
        else
            rc=$?
        fi
        if [ "$rc" -eq 2 ]; then
            echo "    $name: PID $pid is alive but ownership cannot be verified"
        else
            echo "    $name: stale/reused PID $pid"
        fi
        return 1
    fi
    if group_alive "$pid"; then
        echo "    $name: leader exited but process group $pid remains"
    else
        echo "    $name: stopped (stale PID $pid)"
    fi
    return 1
}

show_status() {
    local failed=0
    echo "==> Launcher status"
    if [ -f "$SETTINGS_FILE" ]; then
        sed 's/^/    /' "$SETTINGS_FILE"
    else
        echo "    no saved settings"
    fi
    local pid
    if pid="$(read_pid "$SUPERVISOR_PID_FILE" 2>/dev/null)" && pid_alive "$pid"; then
        if supervisor_owned "$pid"; then
            echo "    supervisor: running (PID $pid)"
        else
            echo "    supervisor: PID $pid is alive but not owned by this launcher"
            failed=1
        fi
    else
        echo "    supervisor: not running"
        [ -f "$READY_FILE" ] && failed=1
    fi
    component_status core || failed=1
    component_status web || failed=1
    component_status nostr || failed=1
    [ -f "$FAILED_FILE" ] && {
        echo "    last failure: $(cat "$FAILED_FILE")"
        failed=1
    }
    return "$failed"
}

follow_logs() {
    local name="${1:-core}"
    case "$name" in
        core|web|nostr|launcher) ;;
        *) die "unknown log component: $name" ;;
    esac
    local file="$LOG_DIR/$name.log"
    [ -f "$file" ] || die "log does not exist: $file"
    tail -n 100 -f "$file"
}

init_paths

case "${1:-}" in
    __supervise)
        supervise
        ;;
    start)
        load_start_config
        build_components
        run_with_lifecycle_lock launch_supervisor
        ;;
    stop)
        run_with_lifecycle_lock stop_all
        ;;
    restart)
        load_start_config
        build_components
        run_with_lifecycle_lock restart_all
        ;;
    status)
        show_status
        ;;
    logs)
        follow_logs "${2:-core}"
        ;;
    *)
        usage
        exit 1
        ;;
esac
