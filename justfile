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
    ETR_PID=$(pgrep -x etr | head -1)
    if [[ -z "$ETR_PID" ]]; then
        echo "FAIL: could not find etr process for reconnect test" >&2
        exit 1
    fi
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

    echo ""
    echo "==> All tests passed."

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
