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

# Verify tools needed for e2e-local (tmux, ssh, passwordless localhost access)
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
#
# etr is launched as the tmux session command (not via send-keys) to avoid
# the .zshrc startup race and so that #{pane_pid} == etr's PID directly.
e2e-local: check-tools install
    #!/usr/bin/env bash
    set -euo pipefail

    CLIENT_LOG="${XDG_STATE_HOME:-$HOME/.local/state}/etr/etr.log"

    cleanup() {
        echo ""
        echo "--- cleanup ---"
        tmux kill-session -t "{{TMUX_SESS}}" 2>/dev/null && echo "killed tmux session {{TMUX_SESS}}" || true
        pkill -x etrs 2>/dev/null && echo "stopped etrs" || true
    }
    trap cleanup EXIT

    mkdir -p "$(dirname "$CLIENT_LOG")"
    # Truncate the client log so session-ready detection isn't confused by
    # a "[etr] Connected." line left over from a previous run.
    > "$CLIENT_LOG"

    # ── 1. Launch etr directly as the tmux session command ───────────────────
    # Running etr as the session command (not via send-keys) avoids the .zshrc
    # startup race and makes #{pane_pid} == etr's PID.
    echo "==> Launching etr client in tmux session '{{TMUX_SESS}}'..."
    tmux new-session -d -s "{{TMUX_SESS}}" -x 200 -y 50 -- \
        "{{INSTALL}}/etr" -v localhost

    # ── 2. Wait for "[etr] Connected." in the client log ─────────────────────
    echo "    waiting for etr to connect..."
    READY=0
    for i in $(seq 1 30); do
        sleep 1
        grep -q '\[etr\] Connected\.' "$CLIENT_LOG" 2>/dev/null && { READY=1; break; }
    done
    if [[ $READY -eq 0 ]]; then
        echo "ERROR: '[etr] Connected.' not seen in $CLIENT_LOG within 30 s" >&2
        cat "$CLIENT_LOG" >&2
        exit 1
    fi

    # Send a sentinel to the remote shell and wait for it to echo back,
    # confirming the PTY stream is live end-to-end.
    SENTINEL="ETR_TEST_READY_$$"
    tmux send-keys -t "{{TMUX_SESS}}" "echo ${SENTINEL}" Enter
    echo "    waiting for remote shell sentinel..."
    READY=0
    for i in $(seq 1 20); do
        sleep 1
        tmux capture-pane -t "{{TMUX_SESS}}" -p -S - 2>/dev/null \
            | grep -q "${SENTINEL}" && { READY=1; break; }
    done
    if [[ $READY -eq 0 ]]; then
        echo "ERROR: remote shell sentinel not seen within 20 s" >&2
        tmux capture-pane -t "{{TMUX_SESS}}" -p -S - >&2
        exit 1
    fi
    echo "    session up."

    # ── 3. Happy-path test ───────────────────────────────────────────────────
    echo "==> Sending test commands..."
    tmux send-keys -t "{{TMUX_SESS}}" "echo HELLO_FROM_ETR && hostname && date" Enter
    sleep 2

    OUTPUT=$(tmux capture-pane -t "{{TMUX_SESS}}" -p -S -)
    if echo "$OUTPUT" | grep -q "HELLO_FROM_ETR"; then
        echo "    PASS: test command output received through etr session."
    else
        echo "FAIL: expected 'HELLO_FROM_ETR' in tmux pane output." >&2
        echo "--- pane output ---" >&2
        echo "$OUTPUT" >&2
        exit 1
    fi

    # ── 4. Reconnect test ────────────────────────────────────────────────────
    # etr is the direct tmux session command, so #{pane_pid} == etr's PID.
    # No need to search with pgrep.
    ETR_PID=$(tmux display-message -t "{{TMUX_SESS}}" -p '#{pane_pid}' 2>/dev/null || true)
    ETR_COMM=$(ps -o comm= -p "${ETR_PID:-0}" 2>/dev/null | tr -d ' ' || true)
    if [[ "$ETR_COMM" != "etr" ]]; then
        echo "SKIP: #{pane_pid}=${ETR_PID:-} is '${ETR_COMM}', not 'etr'; skipping reconnect test" >&2
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

        OUTPUT2=$(tmux capture-pane -t "{{TMUX_SESS}}" -p -S -)
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
    # Run etr DIRECTLY as the tmux session command (no intermediate shell).
    # This avoids .zshrc startup delays and ensures the -L flags are received
    # by the correct etr process, not by a nested remote shell.
    echo "==> etr -L ${TCP_FWD_PORT}:localhost:${TCP_ECHO_PORT} -L ${UDP_FWD_PORT}:localhost:${UDP_ECHO_PORT}/udp localhost"
    tmux new-session -d -s "$STRESS_SESS" -x 220 -y 50 -- \
        "{{INSTALL}}/etr" -v \
        -L "${TCP_FWD_PORT}:localhost:${TCP_ECHO_PORT}" \
        -L "${UDP_FWD_PORT}:localhost:${UDP_ECHO_PORT}/udp" \
        localhost

    # Wait for "[etr] Forwarding:" in the log file — set immediately when the
    # -L specs are parsed, before the QUIC connection is opened.
    echo "    waiting for -L specs to appear in log..."
    READY=0
    for i in $(seq 1 30); do
        sleep 1
        grep -q "Forwarding: ${TCP_FWD_PORT}:" ~/.local/state/etr/etr.log 2>/dev/null && { READY=1; break; }
    done
    [[ $READY -eq 0 ]] && { echo "ERROR: [etr] Forwarding: ${TCP_FWD_PORT} not seen in log" >&2; exit 1; }
    echo "    etr started with -L specs."

    # Send a sentinel to the remote shell and wait for it to echo back.
    SENTINEL="ETR_STRESS_READY_$$"
    tmux send-keys -t "$STRESS_SESS" "echo ${SENTINEL}" Enter
    echo "    waiting for remote shell sentinel..."
    READY=0
    for i in $(seq 1 30); do
        sleep 1
        tmux capture-pane -t "$STRESS_SESS" -p -S - 2>/dev/null \
            | grep -q "${SENTINEL}" && { READY=1; break; }
    done
    [[ $READY -eq 0 ]] && { echo "ERROR: remote shell sentinel not seen" >&2; exit 1; }
    echo "    session up."

    # ── Locate etrs via the remote shell's parent PID ────────────────────────
    # etrs is the direct parent of the remote shell (portable-pty fork+exec).
    # We read /proc/$$/status inside the remote shell ($$ = shell PID, not
    # the grep subprocess), so PPid gives the shell's parent = etrs.
    # Use \$\$ so bash doesn't expand $$ before the command reaches the shell.
    PPID_FILE="/tmp/.etr_stress_ppid_$$"
    tmux send-keys -t "$STRESS_SESS" \
        "awk '/^PPid:/{print \$2}' /proc/\$\$/status > ${PPID_FILE} && echo PPID_OK" Enter
    # Wait for PPID_OK in pane to confirm the command completed
    for i in $(seq 1 10); do
        sleep 1
        tmux capture-pane -t "$STRESS_SESS" -p -S - 2>/dev/null | grep -q "PPID_OK" && break
    done
    ETRS_PID=$(cat "$PPID_FILE" 2>/dev/null | tr -d '[:space:]' || true)
    rm -f "$PPID_FILE"
    if [[ -z "$ETRS_PID" ]] || ! kill -0 "$ETRS_PID" 2>/dev/null; then
        echo "ERROR: cannot locate etrs (PPID method failed; got '${ETRS_PID:-}')" >&2
        exit 1
    fi
    # Sanity-check: the PID should be named "etrs"
    ETRS_COMM=$(ps -o comm= -p "$ETRS_PID" 2>/dev/null | tr -d ' ' || true)
    if [[ "$ETRS_COMM" != "etrs" ]]; then
        echo "ERROR: PID $ETRS_PID is '$ETRS_COMM', not 'etrs' — PPID lookup landed on wrong process" >&2
        exit 1
    fi
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
