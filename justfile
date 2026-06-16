# SPDX-License-Identifier: GPL-3.0-or-later
# etr local test harness

ETR_BIN   := justfile_directory() + "/target/debug/etr"
ETRS_BIN  := justfile_directory() + "/target/debug/etrs"
ETR_REL   := justfile_directory() + "/target/release/etr"
ETRS_REL  := justfile_directory() + "/target/release/etrs"
INSTALL   := home_directory() + "/.cargo/bin"
LOG_FILE  := `echo "${XDG_STATE_HOME:-$HOME/.local/state}/etr/etrs.log"`
SOCK_FILE := `echo "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/etr/etrs.sock"`
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
test-local: check-tools install
    #!/usr/bin/env bash
    set -euo pipefail

    ETRS_PID=""

    cleanup() {
        echo ""
        echo "--- cleanup ---"
        [[ -n "$ETRS_PID" ]] && kill "$ETRS_PID" 2>/dev/null && echo "stopped etrs (pid $ETRS_PID)"
        tmux kill-session -t "{{TMUX_SESS}}" 2>/dev/null && echo "killed tmux session {{TMUX_SESS}}" || true
        rm -f "{{SOCK_FILE}}"
    }
    trap cleanup EXIT

    # ── 1. Start daemon ───────────────────────────────────────────────────────
    echo "==> Starting etrs daemon..."
    mkdir -p "$(dirname "{{LOG_FILE}}")"
    rm -f "{{SOCK_FILE}}"
    "{{ETRS_BIN}}" daemon > "{{LOG_FILE}}" 2>&1 &
    ETRS_PID=$!
    echo "    etrs pid: $ETRS_PID  log: {{LOG_FILE}}"

    # Wait for daemon to be ready (socket appears)
    for i in $(seq 1 20); do
        [[ -S "{{SOCK_FILE}}" ]] && break
        sleep 0.2
    done
    if [[ ! -S "{{SOCK_FILE}}" ]]; then
        echo "ERROR: daemon socket {{SOCK_FILE}} did not appear" >&2
        cat "{{LOG_FILE}}" >&2
        exit 1
    fi
    echo "    daemon ready."

    # ── 2. Launch client in tmux ─────────────────────────────────────────────
    echo "==> Launching etr client in tmux session '{{TMUX_SESS}}'..."
    tmux new-session -d -s "{{TMUX_SESS}}" -x 200 -y 50
    tmux send-keys -t "{{TMUX_SESS}}" "PATH=\"{{INSTALL}}:$PATH\" \"{{ETR_BIN}}\" -v localhost" Enter
    sleep 5  # allow SSH bootstrap + handshake

    # ── 3. Happy-path test ───────────────────────────────────────────────────
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
        echo "--- server log ---" >&2
        cat "{{LOG_FILE}}" >&2
        exit 1
    fi

    # ── 4. Reconnect test ────────────────────────────────────────────────────
    echo "==> Reconnect test: suspending etrs for 17 s (simulates network loss)..."
    kill -STOP "$ETRS_PID"
    echo "    etrs suspended (SIGSTOP). Client will hit 15-s idle timeout..."
    sleep 17
    kill -CONT "$ETRS_PID"
    echo "    etrs resumed (SIGCONT). Waiting for client reconnect..."
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
        echo "--- server log ---" >&2
        cat "{{LOG_FILE}}" >&2
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
    @echo "cleaned up"
