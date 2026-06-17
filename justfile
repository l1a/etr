# SPDX-License-Identifier: GPL-3.0-or-later
# etr local test harness

ETR_BIN   := justfile_directory() + "/target/debug/etr"
ETRS_BIN  := justfile_directory() + "/target/debug/etrs"
ETR_REL   := justfile_directory() + "/target/release/etr"
ETRS_REL  := justfile_directory() + "/target/release/etrs"
INSTALL   := home_directory() + "/.cargo/bin"
LOG_FILE  := `echo "${XDG_STATE_HOME:-$HOME/.local/state}/etr/etrs.log"`
TMUX_SESS := "etr_test"

# List available recipes
default:
    @just --list

# ── Code quality ──────────────────────────────────────────────────────────────

# Format source files
fmt:
    cargo fmt

# Check formatting without modifying files
fmt-check:
    cargo fmt --check

# Run Clippy (deny warnings, check all targets)
clippy:
    cargo clippy --all-targets -- -D warnings

# Run unit and integration tests
test:
    cargo test

# Run security audit on dependencies (installs cargo-audit if absent)
audit:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! cargo audit --version >/dev/null 2>&1; then
        echo "==> Installing cargo-audit..."
        cargo install cargo-audit
    fi
    cargo audit

# Run all static checks: fmt + clippy (suitable as a pre-push gate)
check: fmt-check clippy
    @echo "All checks passed."

# ── Build ─────────────────────────────────────────────────────────────────────

# Build debug binaries
build:
    cargo build

# Build optimised release binaries
build-release:
    cargo build --release

# ── Install ───────────────────────────────────────────────────────────────────

# Install debug binaries to ~/.cargo/bin (no sudo)
install: build
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "{{INSTALL}}"
    cp "{{ETRS_BIN}}" "{{INSTALL}}/etrs"
    cp "{{ETR_BIN}}"  "{{INSTALL}}/etr"
    echo "Installed etrs and etr (debug) to {{INSTALL}}"

# Install release binaries to ~/.cargo/bin (no sudo)
install-release: build-release
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "{{INSTALL}}"
    cp "{{ETRS_REL}}" "{{INSTALL}}/etrs"
    cp "{{ETR_REL}}"  "{{INSTALL}}/etr"
    echo "Installed etrs and etr (release) to {{INSTALL}}"

# ── Local end-to-end testing ─────────────────────────────────────────────────

# Verify tools needed for test-local (tmux, ssh, passwordless localhost access)
check-tools:
    #!/usr/bin/env bash
    set -euo pipefail
    missing=()
    for cmd in cargo tmux ssh; do
        command -v "$cmd" >/dev/null 2>&1 || missing+=("$cmd")
    done
    if [[ ${#missing[@]} -gt 0 ]]; then
        echo "ERROR: missing required tools: ${missing[*]}" >&2
        echo "  cargo — install from https://rustup.rs" >&2
        echo "  tmux  — install via your package manager (e.g. dnf install tmux)" >&2
        echo "  ssh   — install openssh-clients" >&2
        exit 1
    fi
    # Verify SSH can reach localhost in batch mode (no password prompt)
    if ! ssh -q -o BatchMode=yes -o ConnectTimeout=3 localhost true 2>/dev/null; then
        echo "WARNING: SSH to localhost failed." >&2
        echo "  etr's SSH bootstrap requires passwordless SSH to the target host." >&2
        echo "  Run: ssh-copy-id localhost  (or append ~/.ssh/id_*.pub to ~/.ssh/authorized_keys)" >&2
        exit 1
    fi
    echo "All required tools present and SSH to localhost is functional."

# Run the full local end-to-end test (happy path + reconnect)
#
# etr SSHes to localhost, starts etrs on the fly (no pre-running daemon),
# etrs forks and orphans its child, which handles the session.
#
# Reconnect is tested by SIGSTOP-ing the etr client: the tokio event loop
# freezes (no heartbeats sent/received).  After 17 s etrs times out; when
# SIGCONT wakes etr, tokio immediately sees the elapsed 15-s heartbeat
# deadline and reconnects.  No need to locate the etrs process at all.
test-local: check-tools install
    #!/usr/bin/env bash
    set -euo pipefail

    cleanup() {
        echo ""
        echo "--- cleanup ---"
        tmux kill-session -t "{{TMUX_SESS}}" 2>/dev/null && echo "killed tmux session {{TMUX_SESS}}" || true
        pkill -x etrs 2>/dev/null && echo "stopped etrs" || true
    }
    trap cleanup EXIT

    mkdir -p "$(dirname "{{LOG_FILE}}")"

    # ── 1. Launch client in tmux ─────────────────────────────────────────────
    echo "==> Launching etr client in tmux session '{{TMUX_SESS}}'..."
    tmux new-session -d -s "{{TMUX_SESS}}" -x 200 -y 50
    tmux send-keys -t "{{TMUX_SESS}}" "\"{{INSTALL}}/etr\" -v localhost" Enter

    # Wait for the remote shell prompt to appear (indicates handshake is done).
    # We look for a non-empty line whose last visible character is a common
    # prompt terminator: $  %  #  >  ❯  ➜
    echo "    waiting for remote shell prompt..."
    READY=0
    for i in $(seq 1 30); do
        sleep 1
        if tmux capture-pane -t "{{TMUX_SESS}}" -p 2>/dev/null \
                | grep -qE '[❯➜$%#>][[:space:]]'; then
            READY=1
            break
        fi
    done
    if [[ $READY -eq 0 ]]; then
        echo "ERROR: remote shell prompt did not appear within 30 s" >&2
        tmux capture-pane -t "{{TMUX_SESS}}" -p >&2
        exit 1
    fi
    echo "    session ready."

    # ── 2. Happy-path test ───────────────────────────────────────────────────
    echo "==> Sending test commands..."
    tmux send-keys -t "{{TMUX_SESS}}" "echo HELLO_FROM_ETR && hostname && date" Enter
    sleep 2

    OUTPUT=$(tmux capture-pane -t "{{TMUX_SESS}}" -p)
    if echo "$OUTPUT" | grep -q "HELLO_FROM_ETR"; then
        echo "    PASS: test command output received through etr session."
    else
        echo "FAIL: expected 'HELLO_FROM_ETR' in tmux pane output." >&2
        echo "--- pane output ---" >&2
        echo "$OUTPUT" >&2
        exit 1
    fi

    # ── 3. Reconnect test ────────────────────────────────────────────────────
    # SIGSTOP the etr client: its tokio runtime freezes, so no heartbeats are
    # exchanged.  After 17 s (> the 15 s idle timeout), SIGCONT wakes it;
    # tokio sees the elapsed deadline and reconnects to etrs.
    #
    # pgrep -x only matches the binary name, which can fail when invoked from
    # a non-interactive shell.  Find etr as the direct child of the tmux pane's
    # shell process instead.
    PANE_PID=$(tmux display-message -t "{{TMUX_SESS}}" -p '#{pane_pid}' 2>/dev/null || echo "")
    ETR_PID=""
    for i in $(seq 1 5); do
        ETR_PID=$(ps --ppid "${PANE_PID:-0}" -o pid= 2>/dev/null | head -1 | tr -d ' ')
        [[ -n "$ETR_PID" ]] && break
        # fallback: search by install path
        ETR_PID=$(pgrep -f "{{INSTALL}}/etr" | head -1 || true)
        [[ -n "$ETR_PID" ]] && break
        sleep 1
    done
    if [[ -z "$ETR_PID" ]]; then
        echo "SKIP: could not locate etr PID; skipping reconnect test" >&2
    else
        echo "==> Reconnect test: suspending etr client (pid $ETR_PID) for 17 s..."
        kill -STOP "$ETR_PID"
        echo "    etr suspended (SIGSTOP). etrs will hit 15-s idle timeout..."
        sleep 17
        kill -CONT "$ETR_PID"
        echo "    etr resumed (SIGCONT). Waiting for reconnect..."
        sleep 6

        tmux send-keys -t "{{TMUX_SESS}}" "echo RECONNECT_OK && uptime" Enter
        sleep 2

        OUTPUT2=$(tmux capture-pane -t "{{TMUX_SESS}}" -p)
        if echo "$OUTPUT2" | grep -q "RECONNECT_OK"; then
            echo "    PASS: session resumed after reconnect."
        else
            echo "FAIL: expected 'RECONNECT_OK' after reconnect." >&2
            echo "--- pane output ---" >&2
            echo "$OUTPUT2" >&2
            exit 1
        fi
    fi

    echo ""
    echo "==> All tests passed."

# Stress-test all three stream types simultaneously while watching etrs memory.
#
# Opens: 1 PTY stream + 2 -L forward streams (TCP echo + UDP echo).
# Pushes data as fast as possible in both directions on all three streams for
# DURATION seconds, sampling etrs RSS every 2 s.  Fails if etrs grows by more
# than 20 MB above its baseline (well above the 4 MB send-history cap + quinn
# buffers).
#
# Requires: python3, tmux, passwordless SSH to localhost.
stress-local: check-tools install
    #!/usr/bin/env bash
    set -euo pipefail

    TCP_ECHO_PORT=19292
    TCP_FWD_PORT=19291
    UDP_ECHO_PORT=19294
    UDP_FWD_PORT=19293
    DURATION=30
    STRESS_SESS="etr_stress"

    TCP_ECHO_PID="" UDP_ECHO_PID="" TCP_PUMP_PID="" UDP_PUMP_PID=""
    SCRIPTS="{{justfile_directory()}}/scripts/stress"

    cleanup() {
        echo "--- cleanup ---"
        kill "$TCP_ECHO_PID" "$UDP_ECHO_PID" "$TCP_PUMP_PID" "$UDP_PUMP_PID" 2>/dev/null || true
        tmux kill-session -t "$STRESS_SESS" 2>/dev/null || true
        pkill -x etrs 2>/dev/null || true
    }
    trap cleanup EXIT

    mkdir -p "$(dirname "{{LOG_FILE}}")"

    # ── Echo servers ──────────────────────────────────────────────────────────
    echo "==> TCP echo server on :${TCP_ECHO_PORT}..."
    python3 "$SCRIPTS/tcp_echo.py" "$TCP_ECHO_PORT" &
    TCP_ECHO_PID=$!

    echo "==> UDP echo server on :${UDP_ECHO_PORT}..."
    python3 "$SCRIPTS/udp_echo.py" "$UDP_ECHO_PORT" &
    UDP_ECHO_PID=$!
    sleep 0.3

    # ── Connect etr: 1 PTY + 2 -L streams ────────────────────────────────────
    echo "==> etr -L ${TCP_FWD_PORT}:localhost:${TCP_ECHO_PORT} -L ${UDP_FWD_PORT}:localhost:${UDP_ECHO_PORT}/udp localhost"
    tmux new-session -d -s "$STRESS_SESS" -x 220 -y 50
    tmux send-keys -t "$STRESS_SESS" \
        "\"{{INSTALL}}/etr\" -v \
          -L ${TCP_FWD_PORT}:localhost:${TCP_ECHO_PORT} \
          -L ${UDP_FWD_PORT}:localhost:${UDP_ECHO_PORT}/udp \
          localhost" Enter

    echo "    waiting for remote shell..."
    READY=0
    for i in $(seq 1 30); do
        sleep 1
        tmux capture-pane -t "$STRESS_SESS" -p 2>/dev/null \
            | grep -qE '[❯➜$%#>][[:space:]]' && { READY=1; break; }
    done
    [[ $READY -eq 0 ]] && { echo "ERROR: shell prompt not seen" >&2; exit 1; }
    echo "    session up."

    # ── Locate etrs ───────────────────────────────────────────────────────────
    ETRS_PID=""
    for i in $(seq 1 5); do
        ETRS_PID=$(pgrep -x etrs | head -1 || true)
        [[ -n "$ETRS_PID" ]] && break; sleep 1
    done
    [[ -z "$ETRS_PID" ]] && { echo "ERROR: cannot find etrs" >&2; exit 1; }
    RSS_START=$(ps -o rss= -p "$ETRS_PID" | tr -d ' ')
    echo "==> etrs PID=$ETRS_PID  RSS_start=${RSS_START} KB"

    # ── PTY stress: heavy output server→client; sink stdin client→server ──────
    tmux send-keys -t "$STRESS_SESS" \
        "dd if=/dev/urandom bs=65536 2>/dev/null | base64 > /dev/null & dd if=/dev/urandom bs=65536 of=/dev/null 2>/dev/null &" Enter
    sleep 0.5

    # ── TCP and UDP pumps ─────────────────────────────────────────────────────
    echo "==> TCP pump on :${TCP_FWD_PORT}..."
    python3 "$SCRIPTS/tcp_pump.py" "$TCP_FWD_PORT" &
    TCP_PUMP_PID=$!

    echo "==> UDP pump on :${UDP_FWD_PORT}..."
    python3 "$SCRIPTS/udp_pump.py" "$UDP_FWD_PORT" &
    UDP_PUMP_PID=$!

    # ── Sample RSS every 2 s ──────────────────────────────────────────────────
    echo ""
    printf "  %-6s  %-10s  %-10s\n" "t(s)" "RSS(KB)" "growth(KB)"
    printf "  %-6s  %-10s  %-10s\n" "0" "$RSS_START" "0"
    RSS_MAX=$RSS_START
    ETRS_DIED=0

    for t in $(seq 2 2 $DURATION); do
        sleep 2
        if ! kill -0 "$ETRS_PID" 2>/dev/null; then
            echo "FAIL: etrs died at t=${t}s" >&2; ETRS_DIED=1; break
        fi
        if ! tmux has-session -t "$STRESS_SESS" 2>/dev/null; then
            echo "FAIL: tmux session (etr) disappeared at t=${t}s" >&2; ETRS_DIED=1; break
        fi
        RSS=$(ps -o rss= -p "$ETRS_PID" | tr -d ' ')
        GROWTH=$(( RSS - RSS_START ))
        [[ $RSS -gt $RSS_MAX ]] && RSS_MAX=$RSS
        printf "  %-6s  %-10s  %-10s\n" "$t" "$RSS" "$GROWTH"
    done

    [[ $ETRS_DIED -eq 1 ]] && exit 1

    # ── Kill background PTY flood, check etr still responds ──────────────────
    tmux send-keys -t "$STRESS_SESS" 'kill $(jobs -p) 2>/dev/null; echo STRESS_OK' Enter
    sleep 2
    PANE=$(tmux capture-pane -t "$STRESS_SESS" -p 2>/dev/null)
    if ! echo "$PANE" | grep -q "STRESS_OK"; then
        echo "FAIL: etr not responsive after ${DURATION}s stress test" >&2
        echo "$PANE" >&2
        exit 1
    fi
    echo "    etr responsive after stress."

    # ── Verdict ───────────────────────────────────────────────────────────────
    RSS_FINAL=$(ps -o rss= -p "$ETRS_PID" 2>/dev/null | tr -d ' ' || echo 0)
    GROWTH_FINAL=$(( RSS_FINAL - RSS_START ))
    GROWTH_MAX=$(( RSS_MAX - RSS_START ))
    echo ""
    echo "==> etrs RSS: start=${RSS_START}KB  max=${RSS_MAX}KB  final=${RSS_FINAL}KB"
    echo "    peak growth = ${GROWTH_MAX}KB   final growth = ${GROWTH_FINAL}KB"

    # 4 MB send-history cap + quinn buffers + overhead → allow 20 MB headroom
    LIMIT_KB=20480
    if [[ $GROWTH_MAX -gt $LIMIT_KB ]]; then
        echo "FAIL: etrs peak RSS grew by ${GROWTH_MAX}KB (> ${LIMIT_KB}KB limit)" >&2
        exit 1
    fi
    echo "PASS: etrs memory bounded (peak growth ${GROWTH_MAX}KB < ${LIMIT_KB}KB limit)."

# Show live server log
log:
    @mkdir -p "$(dirname "{{LOG_FILE}}")"
    @tail -f "{{LOG_FILE}}"

# Kill daemon and tmux session (manual cleanup)
clean:
    -pkill -x etrs 2>/dev/null
    -tmux kill-session -t "{{TMUX_SESS}}" 2>/dev/null
    cargo clean
    @echo "cleaned up"
