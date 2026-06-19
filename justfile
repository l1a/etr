# SPDX-License-Identifier: GPL-3.0-or-later
# etr local test harness

ETR_BIN    := justfile_directory() + "/target/debug/etr"
ETRS_BIN   := justfile_directory() + "/target/debug/etrs"
ETR_REL    := justfile_directory() + "/target/release/etr"
ETRS_REL   := justfile_directory() + "/target/release/etrs"
STRESS_BIN := justfile_directory() + "/tools/stress/target/release/stress_tool"
INSTALL    := home_directory() + "/.cargo/bin"
LOG_FILE   := `echo "${XDG_STATE_HOME:-$HOME/.local/state}/etr/etrs.log"`
MAN_DIR    := `echo "${XDG_DATA_HOME:-$HOME/.local/share}/man"`
TMUX_SESS  := "etr_test"

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

# Run performance benchmarks
bench:
    cargo bench

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

# Publish to crates.io (dry-run first; aborts if dry-run fails)
publish:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "==> Verifying working tree is clean..."
    if ! git diff --quiet || ! git diff --cached --quiet; then
        echo "ERROR: working tree has uncommitted changes. Commit or discard them first." >&2
        exit 1
    fi
    echo "==> Running cargo publish --dry-run..."
    if ! cargo publish --dry-run; then
        echo "ERROR: dry-run failed — not publishing." >&2
        exit 1
    fi
    echo "==> Dry-run passed. Publishing to crates.io..."
    cargo publish
    echo "==> Published $(grep '^version' Cargo.toml | head -1 | sed 's/.*\"\(.*\)\"/\1/') to crates.io."

# ── Build ─────────────────────────────────────────────────────────────────────

# Build debug binaries
build:
    cargo build

# Build optimised release binaries
build-release:
    cargo build --release

# Build the stress-test helper binary (TCP/UDP echo servers + pumps)
build-stress:
    cargo build --release --manifest-path tools/stress/Cargo.toml

# ── Install ───────────────────────────────────────────────────────────────────

# Install debug binaries to ~/.cargo/bin (no sudo)
install: build
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "{{INSTALL}}"
    cp "{{ETRS_BIN}}" "{{INSTALL}}/etrs"
    cp "{{ETR_BIN}}"  "{{INSTALL}}/etr"
    echo "Installed etrs and etr (debug) to {{INSTALL}}"

# Install release binaries to ~/.cargo/bin and man pages to XDG man dir
install-release: build-release install-man
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "{{INSTALL}}"
    cp "{{ETRS_REL}}" "{{INSTALL}}/etrs"
    cp "{{ETR_REL}}"  "{{INSTALL}}/etr"
    echo "Installed etrs and etr (release) to {{INSTALL}}"

# ── Man pages ────────────────────────────────────────────────────────────────

# Build man pages from man/*.md using pandoc
man:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v pandoc >/dev/null 2>&1; then
        echo "ERROR: pandoc is required to build man pages" >&2
        exit 1
    fi
    VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
    mkdir -p man/build
    pandoc -s -t man --metadata=footer:"etr $VERSION" man/etr.1.md  -o man/build/etr.1
    pandoc -s -t man --metadata=footer:"etr $VERSION" man/etrs.1.md -o man/build/etrs.1
    echo "Built man/build/etr.1 and man/build/etrs.1 (version $VERSION)"

# Install man pages to XDG local man directory (~/.local/share/man/man1)
install-man: man
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "{{MAN_DIR}}/man1"
    cp man/build/etr.1  "{{MAN_DIR}}/man1/etr.1"
    cp man/build/etrs.1 "{{MAN_DIR}}/man1/etrs.1"
    echo "Installed etr.1 and etrs.1 to {{MAN_DIR}}/man1"
    echo "Tip: add {{MAN_DIR}} to MANPATH if not already present"

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
        echo "  tmux  — install via your package manager (e.g. brew install tmux / dnf install tmux)" >&2
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
# Reconnect is tested by SIGSTOP-ing the etrs daemon.  etrs has no controlling
# terminal, so SIGSTOP is safe (no SIGHUP risk).  etr keeps running, notices
# the missing heartbeat after 15 s, and starts reconnecting.  QUIC Initial
# packets accumulate in the OS UDP socket buffer while etrs is stopped; when
# etrs resumes (SIGCONT) it processes them and the session is restored.
#
# (Stopping etr instead would not work on macOS: etr is the tmux pane command
# and is attached to a PTY; when stopped, the PTY hangup delivers SIGHUP+SIGCONT
# which kills the process before we can resume it.)
#
# etr is launched as the tmux session command (not via send-keys) to avoid
# the .zshrc startup race.
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
    ETRS_PID=$(pgrep -x etrs 2>/dev/null | head -1 || true)
    if [[ -z "$ETRS_PID" ]]; then
        echo "SKIP: etrs PID not found; skipping reconnect test" >&2
    else
        echo "==> Reconnect test: suspending etrs (pid $ETRS_PID) for 17 s..."
        kill -STOP "$ETRS_PID"
        echo "    etrs suspended. etr will hit 15-s heartbeat timeout and reconnect..."
        sleep 17
        kill -CONT "$ETRS_PID"
        echo "    etrs resumed. Waiting for reconnect..."
        sleep 8

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

# Run the local E2E test for local port forwarding -L (TCP + UDP, IPv4 + IPv6, reconnect)
e2e-forward-local: check-tools install
    #!/usr/bin/env bash
    set -euo pipefail

    CLIENT_LOG="${XDG_STATE_HOME:-$HOME/.local/state}/etr/etr.log"
    TMUX_FORWARD="etr_forward_test"
    TCP_ECHO_PORT=19321
    TCP_FWD_PORT=19322
    UDP_ECHO_PORT=19323
    UDP_FWD_PORT=19324

    cleanup() {
        echo ""
        echo "--- cleanup ---"
        kill "${TCP_ECHO_PID:-}" "${UDP_ECHO_PID:-}" 2>/dev/null || true
        tmux kill-session -t "$TMUX_FORWARD" 2>/dev/null || true
        pkill -x etrs 2>/dev/null || true
    }
    trap cleanup EXIT

    mkdir -p "$(dirname "$CLIENT_LOG")"
    > "$CLIENT_LOG"

    # Start echo servers (these are the "remote" targets reachable via -L).
    # Since client and server are both localhost, they run on the same machine.
    echo "==> Starting TCP echo server on port ${TCP_ECHO_PORT}..."
    python3 "{{justfile_directory()}}/scripts/stress/tcp_echo.py" "${TCP_ECHO_PORT}" &
    TCP_ECHO_PID=$!

    echo "==> Starting UDP echo server on port ${UDP_ECHO_PORT}..."
    python3 "{{justfile_directory()}}/scripts/stress/udp_echo.py" "${UDP_ECHO_PORT}" &
    UDP_ECHO_PID=$!
    sleep 0.5

    # Launch etr with -L specs
    echo "==> Launching etr with -L specs..."
    tmux new-session -d -s "$TMUX_FORWARD" -x 200 -y 50 -- \
        "{{INSTALL}}/etr" -v \
        -L "${TCP_FWD_PORT}:localhost:${TCP_ECHO_PORT}" \
        -L "${UDP_FWD_PORT}:127.0.0.1:${UDP_ECHO_PORT}/udp" \
        localhost

    # Wait for connect
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
    sleep 1.5  # allow -L listeners to bind

    # ── TCP -L (IPv4) ─────────────────────────────────────────────────────────
    echo "==> Testing TCP -L forwarding (IPv4)..."
    TCP_OUT=$(python3 -c '
    import socket
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(3.0)
    s.connect(("127.0.0.1", '"${TCP_FWD_PORT}"'))
    s.sendall(b"FORWARD_TCP_OK\n")
    print(s.recv(1024).decode())
    s.close()
    ' 2>/dev/null || true)
    if [[ "$TCP_OUT" == *"FORWARD_TCP_OK"* ]]; then
        echo "    PASS: TCP -L forwarding (IPv4) functional."
    else
        echo "FAIL: TCP -L forwarding (IPv4) failed. Output: '${TCP_OUT}'" >&2; exit 1
    fi

    # ── TCP -L (IPv6) ─────────────────────────────────────────────────────────
    echo "==> Testing TCP -L forwarding (IPv6)..."
    TCP_OUT_V6=$(python3 -c '
    import socket
    s = socket.socket(socket.AF_INET6, socket.SOCK_STREAM)
    s.settimeout(3.0)
    s.connect(("::1", '"${TCP_FWD_PORT}"'))
    s.sendall(b"FORWARD_TCP_IPV6_OK\n")
    print(s.recv(1024).decode())
    s.close()
    ' 2>/dev/null || true)
    if [[ "$TCP_OUT_V6" == *"FORWARD_TCP_IPV6_OK"* ]]; then
        echo "    PASS: TCP -L forwarding (IPv6) functional."
    else
        echo "FAIL: TCP -L forwarding (IPv6) failed. Output: '${TCP_OUT_V6}'" >&2; exit 1
    fi

    # ── UDP -L (IPv4) ─────────────────────────────────────────────────────────
    echo "==> Testing UDP -L forwarding (IPv4)..."
    UDP_OUT=$(python3 -c '
    import socket
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(3.0)
    s.sendto(b"FORWARD_UDP_OK", ("127.0.0.1", '"${UDP_FWD_PORT}"'))
    try:
        data, _ = s.recvfrom(1024)
        print(data.decode())
    except socket.timeout:
        print("timeout")
    ' 2>/dev/null || true)
    if [[ "$UDP_OUT" == *"FORWARD_UDP_OK"* ]]; then
        echo "    PASS: UDP -L forwarding (IPv4) functional."
    else
        echo "FAIL: UDP -L forwarding (IPv4) failed. Output: '${UDP_OUT}'" >&2; exit 1
    fi

    # ── UDP -L (IPv6) ─────────────────────────────────────────────────────────
    echo "==> Testing UDP -L forwarding (IPv6)..."
    UDP_OUT_V6=$(python3 -c '
    import socket
    s = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
    s.settimeout(3.0)
    s.sendto(b"FORWARD_UDP_IPV6_OK", ("::1", '"${UDP_FWD_PORT}"'))
    try:
        data, _ = s.recvfrom(1024)
        print(data.decode())
    except socket.timeout:
        print("timeout")
    ' 2>/dev/null || true)
    if [[ "$UDP_OUT_V6" == *"FORWARD_UDP_IPV6_OK"* ]]; then
        echo "    PASS: UDP -L forwarding (IPv6) functional."
    else
        echo "FAIL: UDP -L forwarding (IPv6) failed. Output: '${UDP_OUT_V6}'" >&2; exit 1
    fi

    # ── Reconnect test ────────────────────────────────────────────────────────
    ETRS_PID=$(pgrep -x etrs 2>/dev/null | head -1 || true)
    if [[ -z "$ETRS_PID" ]]; then
        echo "SKIP: etrs PID not found; skipping reconnect test" >&2
    else
        echo "==> Reconnect test: suspending etrs (pid $ETRS_PID) for 17 s..."
        kill -STOP "$ETRS_PID"
        echo "    etrs suspended. etr will hit 15-s heartbeat timeout and reconnect..."
        sleep 17
        kill -CONT "$ETRS_PID"
        echo "    etrs resumed. Waiting for reconnect..."
        sleep 8

        echo "==> Verifying TCP -L forwarding after reconnect..."
        TCP_RECON=$(python3 -c '
        import socket
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(5.0)
        s.connect(("127.0.0.1", '"${TCP_FWD_PORT}"'))
        s.sendall(b"FORWARD_TCP_RECONNECT_OK\n")
        print(s.recv(1024).decode())
        s.close()
        ' 2>/dev/null || true)
        if [[ "$TCP_RECON" == *"FORWARD_TCP_RECONNECT_OK"* ]]; then
            echo "    PASS: TCP -L forwarding resumed after reconnect."
        else
            echo "FAIL: TCP -L forwarding not restored after reconnect. Output: '${TCP_RECON}'" >&2; exit 1
        fi

        echo "==> Verifying UDP -L forwarding after reconnect..."
        UDP_RECON=$(python3 -c '
        import socket
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.settimeout(5.0)
        s.sendto(b"FORWARD_UDP_RECONNECT_OK", ("127.0.0.1", '"${UDP_FWD_PORT}"'))
        try:
            data, _ = s.recvfrom(1024)
            print(data.decode())
        except socket.timeout:
            print("timeout")
        ' 2>/dev/null || true)
        if [[ "$UDP_RECON" == *"FORWARD_UDP_RECONNECT_OK"* ]]; then
            echo "    PASS: UDP -L forwarding resumed after reconnect."
        else
            echo "FAIL: UDP -L forwarding not restored after reconnect. Output: '${UDP_RECON}'" >&2; exit 1
        fi
    fi

    echo ""
    echo "==> All -L forward E2E tests passed."

# Run the local E2E test for reverse port forwarding (both TCP and UDP)
e2e-reverse-local: check-tools install
    #!/usr/bin/env bash
    set -euo pipefail

    CLIENT_LOG="${XDG_STATE_HOME:-$HOME/.local/state}/etr/etr.log"
    TMUX_REVERSE="etr_reverse_test"
    TCP_LOCAL_PORT=19301
    TCP_REMOTE_PORT=19302
    UDP_LOCAL_PORT=19303
    UDP_REMOTE_PORT=19304

    cleanup() {
        echo ""
        echo "--- cleanup ---"
        kill "${TCP_ECHO_PID:-}" "${UDP_ECHO_PID:-}" 2>/dev/null || true
        tmux kill-session -t "$TMUX_REVERSE" 2>/dev/null || true
        pkill -x etrs 2>/dev/null || true
    }
    trap cleanup EXIT

    mkdir -p "$(dirname "$CLIENT_LOG")"
    > "$CLIENT_LOG"

    # Start local echo servers on the client side
    echo "==> Starting local TCP echo server on port ${TCP_LOCAL_PORT}..."
    python3 "{{justfile_directory()}}/scripts/stress/tcp_echo.py" "${TCP_LOCAL_PORT}" &
    TCP_ECHO_PID=$!

    echo "==> Starting local UDP echo server on port ${UDP_LOCAL_PORT}..."
    python3 "{{justfile_directory()}}/scripts/stress/udp_echo.py" "${UDP_LOCAL_PORT}" &
    UDP_ECHO_PID=$!
    sleep 0.5

    # Launch etr with reverse forwarding specs
    echo "==> Launching etr with -R specs..."
    tmux new-session -d -s "$TMUX_REVERSE" -x 200 -y 50 -- \
        "{{INSTALL}}/etr" -v \
        -R "${TCP_REMOTE_PORT}:localhost:${TCP_LOCAL_PORT}" \
        -R "${UDP_REMOTE_PORT}:127.0.0.1:${UDP_LOCAL_PORT}/udp" \
        localhost

    # Wait for connect
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

    # Wait for the listeners to bind on the server (localhost)
    sleep 1.5

    # Verify TCP reverse forwarding by sending data to the server's remote port
    echo "==> Testing TCP reverse forwarding..."
    TCP_OUT=$(python3 -c '
    import socket
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(3.0)
    s.connect(("127.0.0.1", '"${TCP_REMOTE_PORT}"'))
    s.sendall(b"REVERSE_TCP_OK\n")
    print(s.recv(1024).decode())
    s.close()
    ' 2>/dev/null || true)

    if [[ "$TCP_OUT" == *"REVERSE_TCP_OK"* ]]; then
        echo "    PASS: TCP reverse forwarding functional."
    else
        echo "FAIL: TCP reverse forwarding check failed. Output: '${TCP_OUT}'" >&2
        exit 1
    fi

    # Verify TCP reverse forwarding over IPv6 loopback
    echo "==> Testing TCP reverse forwarding over IPv6..."
    TCP_OUT_IPV6=$(python3 -c '
    import socket
    s = socket.socket(socket.AF_INET6, socket.SOCK_STREAM)
    s.settimeout(3.0)
    s.connect(("::1", '"${TCP_REMOTE_PORT}"'))
    s.sendall(b"REVERSE_TCP_IPV6_OK\n")
    print(s.recv(1024).decode())
    s.close()
    ' 2>/dev/null || true)

    if [[ "$TCP_OUT_IPV6" == *"REVERSE_TCP_IPV6_OK"* ]]; then
        echo "    PASS: TCP reverse forwarding over IPv6 functional."
    else
        echo "FAIL: TCP reverse forwarding over IPv6 check failed. Output: '${TCP_OUT_IPV6}'" >&2
        exit 1
    fi

    # Verify UDP reverse forwarding
    echo "==> Testing UDP reverse forwarding..."
    UDP_OUT=$(python3 -c '
    import socket
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(3.0)
    s.sendto(b"REVERSE_UDP_OK", ("127.0.0.1", '"${UDP_REMOTE_PORT}"'))
    try:
        data, addr = s.recvfrom(1024)
        print(data.decode())
    except socket.timeout:
        print("timeout")
    ' 2>/dev/null || true)

    if [[ "$UDP_OUT" == *"REVERSE_UDP_OK"* ]]; then
        echo "    PASS: UDP reverse forwarding functional."
    else
        echo "FAIL: UDP reverse forwarding check failed. Output: '${UDP_OUT}'" >&2
        exit 1
    fi

    # Verify UDP reverse forwarding over IPv6 loopback
    echo "==> Testing UDP reverse forwarding over IPv6..."
    UDP_OUT_IPV6=$(python3 -c '
    import socket
    s = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
    s.settimeout(3.0)
    s.sendto(b"REVERSE_UDP_IPV6_OK", ("::1", '"${UDP_REMOTE_PORT}"'))
    try:
        data, addr = s.recvfrom(1024)
        print(data.decode())
    except socket.timeout:
        print("timeout")
    ' 2>/dev/null || true)

    if [[ "$UDP_OUT_IPV6" == *"REVERSE_UDP_IPV6_OK"* ]]; then
        echo "    PASS: UDP reverse forwarding over IPv6 functional."
    else
        echo "FAIL: UDP reverse forwarding over IPv6 check failed. Output: '${UDP_OUT_IPV6}'" >&2
        exit 1
    fi

    # ── Reconnect test ────────────────────────────────────────────────────────
    ETRS_PID=$(pgrep -x etrs 2>/dev/null | head -1 || true)
    if [[ -z "$ETRS_PID" ]]; then
        echo "SKIP: etrs PID not found; skipping reconnect test" >&2
    else
        echo "==> Reconnect test: suspending etrs (pid $ETRS_PID) for 17 s..."
        kill -STOP "$ETRS_PID"
        echo "    etrs suspended. etr will hit 15-s heartbeat timeout and reconnect..."
        sleep 17
        kill -CONT "$ETRS_PID"
        echo "    etrs resumed. Waiting for reconnect..."
        sleep 8

        # etrs re-binds the -R listeners on receiving the reconnected SessionOpen.
        echo "==> Verifying TCP -R forwarding after reconnect..."
        TCP_RECON=$(python3 -c '
        import socket
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(5.0)
        s.connect(("127.0.0.1", '"${TCP_REMOTE_PORT}"'))
        s.sendall(b"REVERSE_TCP_RECONNECT_OK\n")
        print(s.recv(1024).decode())
        s.close()
        ' 2>/dev/null || true)
        if [[ "$TCP_RECON" == *"REVERSE_TCP_RECONNECT_OK"* ]]; then
            echo "    PASS: TCP -R forwarding resumed after reconnect."
        else
            echo "FAIL: TCP -R forwarding not restored after reconnect. Output: '${TCP_RECON}'" >&2; exit 1
        fi

        echo "==> Verifying UDP -R forwarding after reconnect..."
        UDP_RECON=$(python3 -c '
        import socket
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.settimeout(5.0)
        s.sendto(b"REVERSE_UDP_RECONNECT_OK", ("127.0.0.1", '"${UDP_REMOTE_PORT}"'))
        try:
            data, _ = s.recvfrom(1024)
            print(data.decode())
        except socket.timeout:
            print("timeout")
        ' 2>/dev/null || true)
        if [[ "$UDP_RECON" == *"REVERSE_UDP_RECONNECT_OK"* ]]; then
            echo "    PASS: UDP -R forwarding resumed after reconnect."
        else
            echo "FAIL: UDP -R forwarding not restored after reconnect. Output: '${UDP_RECON}'" >&2; exit 1
        fi
    fi

    echo "==> All reverse E2E tests passed."

# Stress-test all five stream types simultaneously while watching etrs memory.
#
# Opens: 1 PTY stream + 2 -L forward streams (TCP + UDP) + 2 -R forward streams
# (TCP + UDP).  Pushes data as fast as possible in both directions on all streams
# for DURATION seconds, sampling etrs RSS every 2 s.  Fails if etrs grows by more
# than 20 MB above its baseline (well above the 4 MB send-history cap + quinn
# buffers).
#
# Requires: python3, tmux, passwordless SSH to localhost.
stress-local: check-tools install build-stress
    #!/usr/bin/env bash
    set -euo pipefail

    TCP_ECHO_PORT=19292   # -L: remote TCP echo target
    TCP_FWD_PORT=19291    # -L: local listener
    UDP_ECHO_PORT=19294   # -L: remote UDP echo target
    UDP_FWD_PORT=19293    # -L: local listener
    TCP_R_ECHO_PORT=19295 # -R: local TCP echo server (client side)
    TCP_R_FWD_PORT=19296  # -R: remote listener (etrs side)
    UDP_R_ECHO_PORT=19297 # -R: local UDP echo server (client side)
    UDP_R_FWD_PORT=19298  # -R: remote listener (etrs side)
    DURATION=30
    STRESS_SESS="etr_stress"

    TCP_ECHO_PID="" UDP_ECHO_PID="" TCP_PUMP_PID="" UDP_PUMP_PID=""
    TCP_R_ECHO_PID="" UDP_R_ECHO_PID="" TCP_R_PUMP_PID="" UDP_R_PUMP_PID=""
    STRESS_BIN="{{STRESS_BIN}}"
    TCP_PUMP_OUT="/tmp/.etr_tcp_pump_$$"
    UDP_PUMP_OUT="/tmp/.etr_udp_pump_$$"
    TCP_R_PUMP_OUT="/tmp/.etr_tcp_r_pump_$$"
    UDP_R_PUMP_OUT="/tmp/.etr_udp_r_pump_$$"

    cleanup() {
        echo "--- cleanup ---"
        kill "$TCP_ECHO_PID" "$UDP_ECHO_PID" "$TCP_PUMP_PID" "$UDP_PUMP_PID" \
             "$TCP_R_ECHO_PID" "$UDP_R_ECHO_PID" "$TCP_R_PUMP_PID" "$UDP_R_PUMP_PID" 2>/dev/null || true
        tmux kill-session -t "$STRESS_SESS" 2>/dev/null || true
        pkill -x etrs 2>/dev/null || true
        rm -f "$TCP_PUMP_OUT" "$UDP_PUMP_OUT" "$TCP_R_PUMP_OUT" "$UDP_R_PUMP_OUT"
    }
    trap cleanup EXIT

    mkdir -p "$(dirname "{{LOG_FILE}}")"

    # ── Echo servers ──────────────────────────────────────────────────────────
    echo "==> TCP echo server (-L target) on :${TCP_ECHO_PORT}..."
    "$STRESS_BIN" tcp-echo "$TCP_ECHO_PORT" &
    TCP_ECHO_PID=$!

    echo "==> UDP echo server (-L target) on :${UDP_ECHO_PORT}..."
    "$STRESS_BIN" udp-echo "$UDP_ECHO_PORT" &
    UDP_ECHO_PID=$!

    echo "==> TCP echo server (-R target) on :${TCP_R_ECHO_PORT}..."
    "$STRESS_BIN" tcp-echo "$TCP_R_ECHO_PORT" &
    TCP_R_ECHO_PID=$!

    echo "==> UDP echo server (-R target) on :${UDP_R_ECHO_PORT}..."
    "$STRESS_BIN" udp-echo "$UDP_R_ECHO_PORT" &
    UDP_R_ECHO_PID=$!
    sleep 0.3

    # ── Connect etr: 1 PTY + 2 -L + 2 -R streams ────────────────────────────
    # Run etr DIRECTLY as the tmux session command (no intermediate shell).
    # This avoids .zshrc startup delays and ensures the flags are received
    # by the correct etr process, not by a nested remote shell.
    echo "==> etr -L ${TCP_FWD_PORT}:localhost:${TCP_ECHO_PORT} -L ${UDP_FWD_PORT}:localhost:${UDP_ECHO_PORT}/udp"
    echo "       -R ${TCP_R_FWD_PORT}:localhost:${TCP_R_ECHO_PORT} -R ${UDP_R_FWD_PORT}:localhost:${UDP_R_ECHO_PORT}/udp localhost"
    tmux new-session -d -s "$STRESS_SESS" -x 220 -y 50 -- \
        "{{INSTALL}}/etr" -v \
        -L "${TCP_FWD_PORT}:localhost:${TCP_ECHO_PORT}" \
        -L "${UDP_FWD_PORT}:localhost:${UDP_ECHO_PORT}/udp" \
        -R "${TCP_R_FWD_PORT}:localhost:${TCP_R_ECHO_PORT}" \
        -R "${UDP_R_FWD_PORT}:localhost:${UDP_R_ECHO_PORT}/udp" \
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
    # Use `ps -o ppid= -p $$` inside the remote shell to get the parent PID —
    # this is POSIX and works on both Linux and macOS (unlike /proc/$$/status).
    # Use \$\$ so bash doesn't expand $$ before the command reaches the shell.
    PPID_FILE="/tmp/.etr_stress_ppid_$$"
    tmux send-keys -t "$STRESS_SESS" \
        "ps -o ppid= -p \$\$ | tr -d '[:space:]' > ${PPID_FILE} && echo PPID_OK" Enter
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

    # ── -L pumps ──────────────────────────────────────────────────────────────
    echo "==> TCP -L pump on :${TCP_FWD_PORT}..."
    "$STRESS_BIN" tcp-pump "$TCP_FWD_PORT" > "$TCP_PUMP_OUT" &
    TCP_PUMP_PID=$!

    echo "==> UDP -L pump on :${UDP_FWD_PORT}..."
    "$STRESS_BIN" udp-pump "$UDP_FWD_PORT" > "$UDP_PUMP_OUT" &
    UDP_PUMP_PID=$!

    # ── -R pumps (connect to etrs-side listeners; brief wait for bind) ────────
    sleep 1.5
    echo "==> TCP -R pump on :${TCP_R_FWD_PORT}..."
    "$STRESS_BIN" tcp-pump "$TCP_R_FWD_PORT" > "$TCP_R_PUMP_OUT" &
    TCP_R_PUMP_PID=$!

    echo "==> UDP -R pump on :${UDP_R_FWD_PORT}..."
    "$STRESS_BIN" udp-pump "$UDP_R_FWD_PORT" > "$UDP_R_PUMP_OUT" &
    UDP_R_PUMP_PID=$!

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

    # ── Throughput report ─────────────────────────────────────────────────────
    # SIGTERM triggers each pump's stats handler; wait ensures the output file
    # is fully written before we read it.
    kill -TERM "$TCP_PUMP_PID" "$UDP_PUMP_PID" "$TCP_R_PUMP_PID" "$UDP_R_PUMP_PID" 2>/dev/null || true
    wait "$TCP_PUMP_PID" "$UDP_PUMP_PID" "$TCP_R_PUMP_PID" "$UDP_R_PUMP_PID" 2>/dev/null || true
    TCP_PUMP_PID="" UDP_PUMP_PID="" TCP_R_PUMP_PID="" UDP_R_PUMP_PID=""  # prevent double-kill in cleanup

    TCP_LINE=$(cat "$TCP_PUMP_OUT" 2>/dev/null || echo "")
    UDP_LINE=$(cat "$UDP_PUMP_OUT" 2>/dev/null || echo "")
    TCP_R_LINE=$(cat "$TCP_R_PUMP_OUT" 2>/dev/null || echo "")
    UDP_R_LINE=$(cat "$UDP_R_PUMP_OUT" 2>/dev/null || echo "")

    echo ""
    echo "==> Throughput (Mb/s = megabits per second):"
    throughput_report() {
        local line="$1" label="$2"
        if [[ -z "$line" ]]; then echo "  ${label}: no stats available"; return; fi
        echo "$line" | awk -v label="$label" '{
            for (i=1; i<=NF; i++) {
                if ($i ~ /^sent=/)    sent    = substr($i, 6) + 0
                if ($i ~ /^recv=/)    recv    = substr($i, 6) + 0
                if ($i ~ /^elapsed=/) elapsed = substr($i, 9) + 0
            }
            if (elapsed <= 0) elapsed = 0.001
            tx = sent * 8 / elapsed / 1000000
            rx = recv * 8 / elapsed / 1000000
            printf "  %-8s tx=%.1f Mb/s  rx=%.1f Mb/s  (%d MiB sent, %d MiB recv in %.1fs)\n", \
                label ":", tx, rx, sent/1048576, recv/1048576, elapsed
        }'
    }
    throughput_report "$TCP_LINE"   "TCP -L"
    throughput_report "$UDP_LINE"   "UDP -L"
    throughput_report "$TCP_R_LINE" "TCP -R"
    throughput_report "$UDP_R_LINE" "UDP -R"

    # ── Verdict ───────────────────────────────────────────────────────────────
    RSS_FINAL=$(ps -o rss= -p "$ETRS_PID" 2>/dev/null | tr -d ' ' || echo 0)
    GROWTH_FINAL=$(( RSS_FINAL - RSS_START ))
    GROWTH_MAX=$(( RSS_MAX - RSS_START ))
    echo ""
    echo "==> etrs RSS: start=${RSS_START}KB  max=${RSS_MAX}KB  final=${RSS_FINAL}KB"
    echo "    peak growth = ${GROWTH_MAX}KB   final growth = ${GROWTH_FINAL}KB"

    # 4 MB send-history cap per stream + 4 MB QUIC stream receive window per stream
    # + quinn connection buffers + overhead.  4 active forward streams × ~8 MB = ~32 MB;
    # allow 48 MB to cover PTY stream and general quinn overhead.
    LIMIT_KB=49152
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
