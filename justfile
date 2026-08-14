# SPDX-License-Identifier: GPL-3.0-or-later
# etr local test harness

# Needed so a shebang recipe receives *ARGS as real argv ($@) rather than losing
# quoting through textual ARGS interpolation -- see open-pr.
set positional-arguments := true

ETR_BIN    := justfile_directory() + "/target/debug/etr"
ETRS_BIN   := justfile_directory() + "/target/debug/etrs"
ETR_REL    := justfile_directory() + "/target/release/etr"
ETRS_REL   := justfile_directory() + "/target/release/etrs"
STRESS_BIN := justfile_directory() + "/tools/stress/target/release/stress_tool"
INSTALL    := home_directory() + "/.cargo/bin"

# ===== PROJECT — the only part of the install family this repo owns =====
#
# etr is the TWO-BINARY case the shared standard exists for: the COMMON block below is
# written against these, so it stays byte-identical to the siblings that ship one binary.
BINS      := "etr etrs"
MAN_PAGES := "man/build/etr.1 man/build/etrs.1"

# Do NOT edit inside the markers. Edit templates/justfile-common.just and the two vendored
# helpers, bump their versions, and propagate to the siblings in their own PRs.
# `just standard-check` runs the helpers' self-tests and `just check` depends on it.
# >>> COMMON (template v3)
# The interpreter is resolved ONCE per line, and a missing one is a hard error. The
# `python3 … 2>/dev/null || python …` idiom is deliberately NOT used: it retries on ANY
# failure, so a real error inside the script gets re-run and reported as if the
# interpreter were the problem.
PY := `command -v python3 || command -v python || echo PYTHON-NOT-FOUND`

# Install from this checkout: binary, man page(s) and completions.
#
# The dependencies are the point. `cargo install` alone replaces the binary and leaves the
# man page and completions at whatever version last ran their recipe — measured on a host
# whose page was ELEVEN releases stale with nothing reporting it.
install: install-man install-completions
    cargo install --path .

# Install a RELEASED tag: binary, man page(s) and completions, all three FROM THAT TAG.
#
# **It deliberately does NOT depend on `install-man`/`install-completions`**, because those
# work from the checkout. Reusing them would pair a tag's binary with the worktree's man
# page and completions — on a checkout one release ahead, a v0.2.22 binary with a v0.2.23
# page. Mismatched artefacts that each look fine is the failure class this standard exists
# to remove, so the three sources are made to agree: binary from the tag, completions from
# THE INSTALLED BINARY (`--from-path`), man page from the tag (`--from-tag`).
#
# Never `--path`: on a Syncthing-shared checkout that builds from a directory other
# machines write into. Takes a bare version and normalises a leading `v`.
install-tag VERSION:
    #!/usr/bin/env bash
    set -euo pipefail
    V="{{VERSION}}"; V="${V#v}"
    [ -n "$V" ] || { echo "error: install-tag needs a version, e.g. just install-tag 0.2.22" >&2; exit 1; }
    git rev-parse -q --verify "refs/tags/v${V}" >/dev/null || {
        echo "error: tag v${V} is not in this clone. Run: git fetch --tags" >&2; exit 1; }
    REPO=$(git config --get remote.origin.url)
    echo "Installing from tag v${V} of ${REPO}"
    cargo install --git "$REPO" --tag "v${V}" --locked --force
    # POST-CONDITION: cargo prints a replacement line, but only a version query proves which
    # binary is on PATH now.
    for b in {{BINS}}; do
        command -v "$b" >/dev/null 2>&1 || { echo "error: $b is not on PATH after install" >&2; exit 1; }
        echo "  $b -> $("$b" --version)"
    done
    "{{PY}}" scripts/install_man.py {{MAN_PAGES}} --from-tag "v${V}"
    "{{PY}}" scripts/install_completions.py {{BINS}} --from-path

# Install the man page(s) to the XDG man directory.
install-man: man
    @"{{PY}}" scripts/install_man.py {{MAN_PAGES}}

# Generate and install shell completions for every binary.
#
# Python rather than a just recipe, which is retch's finding and the more portable
# mechanism: no `sh`, no `cygpath`, no coreutils, nothing from Git's `usr\bin` on Windows.
# A `bash` shebang recipe cannot run on Windows without `cygpath` at all, and even a plain
# `sh` recipe still needs an `sh` on PATH.
install-completions: build
    @"{{PY}}" scripts/install_completions.py {{BINS}}

# Prove the vendored helpers still behave the way the standard requires.
#
# **This runs the helpers' own self-tests rather than diffing text**, and that is the whole
# point: three separate repositories cannot diff each other's files, but each can prove its
# copy still behaves correctly — which is the property that was actually violated when two
# repos quietly shipped the pre-fix nushell path for months. A text diff would also have
# passed happily on a repo that had never adopted the standard at all.
standard-check:
    #!/usr/bin/env bash
    set -euo pipefail
    [ "{{PY}}" != "PYTHON-NOT-FOUND" ] || { echo "error: no python3/python on PATH" >&2; exit 1; }
    "{{PY}}" scripts/install_completions.py --self-test
    "{{PY}}" scripts/install_man.py --self-test
    "{{PY}}" scripts/gate_conformance.py --self-test
    "{{PY}}" scripts/gate_conformance.py "{{justfile()}}"
# <<< COMMON
LOG_FILE   := `echo "${XDG_STATE_HOME:-$HOME/.local/state}/etr/etrs.log"`
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
check: fmt-check clippy standard-check
    @echo "All checks passed."

# Pre-PR gate: run all automated checks and print manual checklist before opening a PR.
# All items must pass before calling `gh pr create`. See AGENTS.md Part 2 §4.
pr:
    #!/usr/bin/env bash
    set -euo pipefail
    BOLD='\033[1m'; GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'; NC='\033[0m'
    pass() { echo -e "${GREEN}[✓]${NC} $1"; }
    fail() { echo -e "${RED}[✗]${NC} $1"; exit 1; }
    info() { echo -e "${YELLOW}[→]${NC} $1"; }

    echo -e "\n${BOLD}=== Pre-PR Gate ===${NC}\n"

    # 1. Must be on a feature branch
    BRANCH=$(git rev-parse --abbrev-ref HEAD)
    [ "$BRANCH" = "main" ] && fail "On main — create a feature branch first"
    pass "Feature branch: $BRANCH"

    # 2. Version must be bumped past the last tag
    CARGO_VER=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
    LAST_TAG=$(git describe --tags --abbrev=0 2>/dev/null || echo "none")
    [ "$LAST_TAG" = "v$CARGO_VER" ] && fail "Version not bumped — Cargo.toml is still $CARGO_VER (matches last tag)"
    pass "Version bumped: $CARGO_VER (last tag: $LAST_TAG)"

    # 3. NOTES.md "Current state" header must match
    grep -q "^## Current state: v$CARGO_VER" NOTES.md \
        || fail "NOTES.md 'Current state' header not updated to v$CARGO_VER"
    pass "NOTES.md Current state header: v$CARGO_VER"

    # 4. Man pages must build cleanly (man/build/ is gitignored, so there is nothing to
    #    diff — this just proves mandown still succeeds and the version header is live).
    info "Building man pages..."
    just man
    pass "Man pages build cleanly"

    # 5. cargo check — updates Cargo.lock; verify it was committed
    info "Running cargo check..."
    cargo check -q 2>&1
    LOCK_DIRTY=$(git diff --name-only Cargo.lock)
    [ -n "$LOCK_DIRTY" ] && fail "Cargo.lock was updated but not committed — stage and commit it first"
    pass "Cargo.lock is current and committed"

    # 6. fmt + clippy
    info "Running just check..."
    just check
    pass "fmt + clippy passed"

    # 7. Tests
    info "Running cargo test..."
    cargo test -q 2>&1
    pass "All tests passed"

    # Manual checklist
    echo -e "\n${BOLD}Automated checks passed.${NC}\n"
    echo -e "${BOLD}Manual checklist — confirm each before proceeding:${NC}"
    echo "  [ ] etr --help / etrs --help reviewed and shell completions regenerate cleanly"
    echo "  [ ] Config file docs updated (config.toml comments + NOTES.md example) if a config key changed"
    echo "  [ ] PROTOCOL.md updated if the wire protocol changed"
    echo "  [ ] README.md reviewed and updated (new features, install steps, platform notes)"
    echo "  [ ] NOTES.md known-gaps section and test-coverage count updated"
    echo "  [ ] GitHub wiki cloned and updated (etr.wiki.git — see AGENTS.md §4.11 for page list)"
    echo ""
    # A bare `read` makes this gate unanswerable by anything that is not a human at a
    # terminal: a script, CI job or agent blocks on a stdin that will never answer, or dies
    # without saying why -- and that reads as the gate REFUSING the change rather than asking
    # a question nobody could hear. Three sources of an answer, in order:
    #
    #   1. PR_CONFIRM in the environment -- the explicit answer for a non-interactive caller.
    #      NOT a bypass: setting it is the same act of confirmation as typing y, just recorded
    #      where a script can supply it. Answer it AFTER checking each item.
    #   2. An interactive stdin -- a human, prompted exactly as before.
    #   3. Neither, so read piped input under a timeout. `echo y | just pr` keeps working, and
    #      a stdin that never answers costs ten seconds rather than hanging.
    #
    # The failure names PR_CONFIRM: a gate that cannot be satisfied from the context it failed
    # in is a wall, not a gate.
    if [ -n "${PR_CONFIRM:-}" ]; then
        CONFIRM="$PR_CONFIRM"
        echo "All manual items confirmed? [y/N] $CONFIRM   (answered by PR_CONFIRM)"
    elif [ -t 0 ]; then
        echo -n "All manual items confirmed? [y/N] "
        read -r CONFIRM
    else
        echo -n "All manual items confirmed? [y/N] "
        read -r -t 10 CONFIRM || CONFIRM=""
        echo "$CONFIRM"
        [ -n "$CONFIRM" ] || { echo -e "${RED}Aborted.${NC} No terminal to confirm the checklist on, and nothing on stdin. Re-run with PR_CONFIRM=y once each item above is actually checked."; exit 1; }
    fi
    [ "$CONFIRM" = "y" ] || [ "$CONFIRM" = "Y" ] \
        || { echo -e "${RED}Aborted.${NC} Complete the checklist first."; exit 1; }

    echo -e "\n${GREEN}Gate passed. You may now run: gh pr create${NC}\n"

# Run the pre-PR gate, then gh pr create -- always use this, never gh pr create directly
open-pr *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    #   just open-pr --title "..." --body-file body.md      # at a terminal
    #   PR_CONFIRM=y just open-pr --title "..." --fill      # script, CI or agent
    #
    # This recipe is the only thing that can gate PR creation: neither `gh` nor `git` has a
    # hook for "a PR is about to open". Being a justfile recipe rather than editor or agent
    # configuration, it binds every contributor and tool identically -- AGENTS.md Part 1 §4.
    # This repo previously asked for that discipline in prose instead, which binds nobody.
    just pr

    # Push the branch if it has no upstream yet. Without this, on a never-pushed branch
    # `gh pr create` has no remote branch to open from and fails non-interactively -- AFTER
    # the gate printed "Gate passed", which reads as the gate rejecting work it just approved.
    #
    # Deliberately ONLY when there is no upstream: pushing unconditionally would silently
    # publish existing commits on a branch that already has one. pre-push runs `just check`,
    # so the push is inside the gate rather than around it.
    if ! git rev-parse --abbrev-ref --symbolic-full-name '@{upstream}' >/dev/null 2>&1; then
        BRANCH="$(git rev-parse --abbrev-ref HEAD)"
        [ "$BRANCH" != HEAD ] || { echo "detached HEAD -- check out a branch first" >&2; exit 1; }
        echo "no upstream for $BRANCH -- pushing it so gh has a remote branch to open from"
        git push -u origin "$BRANCH"
    fi

    # Drop the empty argument just passes when *ARGS is unset.
    ARGS=()
    for a in "$@"; do [ -n "$a" ] && ARGS+=("$a"); done

    # With no arguments and no terminal, gh fails with "must provide --title and --body";
    # --fill uses the commit messages instead so the recipe finishes cleanly.
    if [ ${#ARGS[@]} -eq 0 ] && [ ! -t 0 ]; then
        ARGS=(--fill)
    fi
    gh pr create "${ARGS[@]}"

# Install this repo's tracked git hooks (pre-push runs `just check`)
install-hooks:
    @"{{PY}}" scripts/install_hooks.py

# Merge the active PR, switch to main, pull, delete the branch, and reset WIP.md (requires gh)
merge-pr:
    #!/usr/bin/env bash
    set -euo pipefail
    BRANCH=$(git rev-parse --abbrev-ref HEAD)
    if [ "$BRANCH" = "main" ]; then
        echo "Error: You are already on main."
        exit 1
    fi
    # Refuse to merge over a failing check.
    #
    # `gh pr merge` happily merges a red PR when the repository has no branch protection, and
    # "wait for the checks to settle" is not "wait for them to pass". This repo never had the
    # gate at all, so every merge here has been ungated -- safe only because whoever merged
    # happened to look at CI first.
    echo "Checking CI on this branch..."
    STATES=$(gh pr view --json statusCheckRollup --jq '[.statusCheckRollup[]? | select(.conclusion != "SKIPPED") | .conclusion]' 2>/dev/null || echo '[]')

    # NO checks at all is not "green", and the arm below cannot tell the difference: an empty
    # rollup matches neither "" nor FAILURE, so without this the recipe would report green and
    # merge a commit CI has never seen. Not hypothetical -- it happened in a sibling repo when
    # GitHub stopped creating runs for pushed commits.
    #
    # Compared as a string rather than through `jq -e length`: `gh --jq` is gh's BUILT-IN jq,
    # but an external `jq` is not on a default Windows PATH, and a gate that silently degrades
    # where its dependency is missing is the thing being fixed, not a way to fix it.
    if [ "$(printf '%s' "$STATES" | tr -d '[:space:]')" = "[]" ]; then
        echo "Error: no checks have reported for this commit at all."
        echo "       That is not the same as passing. GitHub sometimes fails to create a run;"
        echo "       force one with: gh workflow run ci.yml --ref $BRANCH"
        exit 1
    fi

    if echo "$STATES" | grep -q '""'; then
        echo "Error: checks are still running. Wait for them, or merge deliberately with gh."
        exit 1
    fi

    if echo "$STATES" | grep -qE 'FAILURE|TIMED_OUT|CANCELLED|ACTION_REQUIRED'; then
        echo "Error: CI is not green on this branch:"
        gh pr view --json statusCheckRollup --jq '.statusCheckRollup[]? | select(.conclusion != "SKIPPED" and .conclusion != "SUCCESS") | "  \(.conclusion)  \(.name)"'
        echo "Fix it, or merge deliberately with gh if you have a reason."
        exit 1
    fi
    echo "CI is green."

    echo "Merging PR for branch $BRANCH..."
    gh pr merge --squash --delete-branch
    echo "Switching to main and pulling..."
    git checkout main
    git pull
    echo "Deleting local branch $BRANCH..."
    git branch -D "$BRANCH" 2>/dev/null || true
    python3 scripts/reset_wip.py

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
    echo "==> Publishing AUR package..."
    just publish-aur

# Publish/update the AUR package (etr-terminal-bin) from the current version's GitHub release
publish-aur:
    #!/usr/bin/env bash
    set -euo pipefail
    VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
    AUR_PKG="etr-terminal-bin"
    AUR_REMOTE="ssh://aur@aur.archlinux.org/${AUR_PKG}.git"
    BASE_URL="https://github.com/l1a/etr/releases/download/v${VERSION}"
    ASSETS=(etr-linux-x86_64 etrs-linux-x86_64 etr-linux-aarch64 etrs-linux-aarch64)

    # The AUR package points at GitHub release assets, so the v$VERSION release
    # must be fully built before this can run (tag → release.yml → here).
    echo "==> Verifying GitHub release v${VERSION} assets exist..."
    for a in "${ASSETS[@]}"; do
        if ! curl -fsIL "${BASE_URL}/${a}" >/dev/null 2>&1; then
            echo "ERROR: ${BASE_URL}/${a} not found." >&2
            echo "The v${VERSION} GitHub release must exist before publishing to the AUR:" >&2
            echo "  git tag v${VERSION} && git push origin v${VERSION}" >&2
            echo "then wait for the Release workflow to finish and re-run: just publish-aur" >&2
            exit 1
        fi
    done

    WORK=$(mktemp -d)
    trap 'rm -rf "$WORK"' EXIT

    echo "==> Downloading release assets and computing sha256 checksums..."
    declare -A SHA
    for a in "${ASSETS[@]}"; do
        curl -fsSL -o "${WORK}/${a}" "${BASE_URL}/${a}"
        SHA[$a]=$(sha256sum "${WORK}/${a}" | cut -d' ' -f1)
        echo "    ${a}  ${SHA[$a]}"
    done

    # Render PKGBUILD and .SRCINFO from the same templates with the same
    # substitutions so the two can never disagree.
    render() {
        sed -e "s/@VERSION@/${VERSION}/g" \
            -e "s/@SHA_ETR_X86_64@/${SHA[etr-linux-x86_64]}/g" \
            -e "s/@SHA_ETRS_X86_64@/${SHA[etrs-linux-x86_64]}/g" \
            -e "s/@SHA_ETR_AARCH64@/${SHA[etr-linux-aarch64]}/g" \
            -e "s/@SHA_ETRS_AARCH64@/${SHA[etrs-linux-aarch64]}/g" \
            "$1"
    }

    echo "==> Cloning ${AUR_REMOTE}..."
    git clone "${AUR_REMOTE}" "${WORK}/aur"
    render "{{justfile_directory()}}/packaging/aur/PKGBUILD.in" > "${WORK}/aur/PKGBUILD"
    render "{{justfile_directory()}}/packaging/aur/SRCINFO.in"  > "${WORK}/aur/.SRCINFO"

    if [ -z "$(git -C "${WORK}/aur" status --porcelain)" ]; then
        echo "==> AUR package already up to date (v${VERSION}); nothing to push."
        exit 0
    fi
    git -C "${WORK}/aur" add PKGBUILD .SRCINFO
    git -C "${WORK}/aur" commit -m "Update to v${VERSION}"
    git -C "${WORK}/aur" push
    echo "==> Published ${AUR_PKG} v${VERSION} to the AUR."

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

# ── Man pages ────────────────────────────────────────────────────────────────

# Build man pages from man/*.md using mandown
man:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v mandown >/dev/null 2>&1; then
        echo "ERROR: mandown is required to build man pages (cargo install mandown)" >&2
        exit 1
    fi
    VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
    mkdir -p man/build
    mandown man/etr.1.md ETR 1  | sed "1s|.*|.TH \"ETR\" \"1\" \"\" \"etr $VERSION\" \"User Commands\"|"  > man/build/etr.1
    mandown man/etrs.1.md ETRS 1 | sed "1s|.*|.TH \"ETRS\" \"1\" \"\" \"etr $VERSION\" \"User Commands\"|" > man/build/etrs.1
    echo "Built man/build/etr.1 and man/build/etrs.1 (version $VERSION)"

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

# Run the local E2E test for --env variable forwarding to the remote shell
e2e-env-local: check-tools install
    #!/usr/bin/env bash
    set -euo pipefail

    CLIENT_LOG="${XDG_STATE_HOME:-$HOME/.local/state}/etr/etr.log"
    TMUX_SESS_ENV="etr_env_test"

    cleanup() {
        echo ""
        echo "--- cleanup ---"
        tmux kill-session -t "$TMUX_SESS_ENV" 2>/dev/null && echo "killed tmux session $TMUX_SESS_ENV" || true
        pkill -x etrs 2>/dev/null && echo "stopped etrs" || true
    }
    trap cleanup EXIT

    mkdir -p "$(dirname "$CLIENT_LOG")"
    > "$CLIENT_LOG"

    # ── 1. Launch etr with --env KEY=VALUE and --env KEY (bare forward) ───────
    echo "==> Launching etr with --env ETR_TEST_SET=hello_env --env ETR_TEST_FWD..."
    export ETR_TEST_FWD="forwarded_value_$$"
    tmux new-session -d -s "$TMUX_SESS_ENV" -x 200 -y 50 -- \
        "{{INSTALL}}/etr" -v \
            --env "ETR_TEST_SET=hello_env" \
            --env "ETR_TEST_FWD" \
            localhost

    # ── 2. Wait for "[etr] Connected." ────────────────────────────────────────
    echo "    waiting for etr to connect..."
    READY=0
    for i in $(seq 1 30); do
        sleep 1
        grep -q '\[etr\] Connected\.' "$CLIENT_LOG" 2>/dev/null && { READY=1; break; }
    done
    if [[ $READY -eq 0 ]]; then
        echo "ERROR: '[etr] Connected.' not seen within 30 s" >&2
        cat "$CLIENT_LOG" >&2
        exit 1
    fi

    # Send a sentinel to confirm the PTY stream is live.
    SENTINEL="ETR_ENV_READY_$$"
    tmux send-keys -t "$TMUX_SESS_ENV" "echo ${SENTINEL}" Enter
    READY=0
    for i in $(seq 1 20); do
        sleep 1
        tmux capture-pane -t "$TMUX_SESS_ENV" -p -S - 2>/dev/null \
            | grep -q "${SENTINEL}" && { READY=1; break; }
    done
    if [[ $READY -eq 0 ]]; then
        echo "ERROR: shell sentinel not seen within 20 s" >&2
        tmux capture-pane -t "$TMUX_SESS_ENV" -p -S - >&2
        exit 1
    fi
    echo "    session up."

    # ── 3. Check KEY=VALUE variable ────────────────────────────────────────────
    echo "==> Checking ETR_TEST_SET=hello_env ..."
    tmux send-keys -t "$TMUX_SESS_ENV" 'echo "ETR_TEST_SET=[$ETR_TEST_SET]"' Enter
    sleep 2
    OUTPUT=$(tmux capture-pane -t "$TMUX_SESS_ENV" -p -S -)
    if echo "$OUTPUT" | grep -q "ETR_TEST_SET=\[hello_env\]"; then
        echo "    PASS: ETR_TEST_SET correctly set to 'hello_env'."
    else
        echo "FAIL: ETR_TEST_SET not found or wrong value." >&2
        echo "--- pane output ---" >&2
        echo "$OUTPUT" >&2
        exit 1
    fi

    # ── 4. Check bare KEY forwarding ──────────────────────────────────────────
    echo "==> Checking ETR_TEST_FWD (bare forward, value: $ETR_TEST_FWD) ..."
    tmux send-keys -t "$TMUX_SESS_ENV" 'echo "ETR_TEST_FWD=[$ETR_TEST_FWD]"' Enter
    sleep 2
    OUTPUT2=$(tmux capture-pane -t "$TMUX_SESS_ENV" -p -S -)
    EXPECTED_FWD="ETR_TEST_FWD=\[${ETR_TEST_FWD}\]"
    if echo "$OUTPUT2" | grep -q "$EXPECTED_FWD"; then
        echo "    PASS: ETR_TEST_FWD correctly forwarded."
    else
        echo "FAIL: ETR_TEST_FWD not found or wrong value (expected '$ETR_TEST_FWD')." >&2
        echo "--- pane output ---" >&2
        echo "$OUTPUT2" >&2
        exit 1
    fi

    echo ""
    echo "==> All --env tests passed."

# Run the local E2E test for remote command execution (etr host 'command')
e2e-cmd-local: check-tools install
    #!/usr/bin/env bash
    set -euo pipefail

    CLIENT_LOG="${XDG_STATE_HOME:-$HOME/.local/state}/etr/etr.log"
    TMUX_SESS_CMD="etr_cmd_test"
    SENTINEL="CMD_TEST_SENTINEL_$$"

    cleanup() {
        echo ""
        echo "--- cleanup ---"
        tmux kill-session -t "$TMUX_SESS_CMD" 2>/dev/null && echo "killed tmux session $TMUX_SESS_CMD" || true
        pkill -x etrs 2>/dev/null && echo "stopped etrs" || true
    }
    trap cleanup EXIT

    mkdir -p "$(dirname "$CLIENT_LOG")"
    > "$CLIENT_LOG"

    # ── 1. Launch etr with a remote command ──────────────────────────────────
    # The command prints a sentinel then sleeps briefly so the pane stays open
    # long enough to capture; the session ends when the command exits.
    echo "==> Launching etr with remote command: echo ${SENTINEL} && sleep 5"
    tmux new-session -d -s "$TMUX_SESS_CMD" -x 200 -y 50 -- \
        "{{INSTALL}}/etr" -v localhost "echo ${SENTINEL} && sleep 5"

    # ── 2. Wait for "[etr] Connected." ────────────────────────────────────────
    echo "    waiting for etr to connect..."
    READY=0
    for i in $(seq 1 30); do
        sleep 1
        grep -q '\[etr\] Connected\.' "$CLIENT_LOG" 2>/dev/null && { READY=1; break; }
    done
    if [[ $READY -eq 0 ]]; then
        echo "ERROR: '[etr] Connected.' not seen within 30 s" >&2
        cat "$CLIENT_LOG" >&2
        exit 1
    fi

    # ── 3. Wait for command output to appear in the pane ─────────────────────
    echo "    waiting for command output..."
    FOUND=0
    for i in $(seq 1 15); do
        sleep 1
        tmux capture-pane -t "$TMUX_SESS_CMD" -p -S - 2>/dev/null \
            | grep -q "${SENTINEL}" && { FOUND=1; break; }
    done
    if [[ $FOUND -eq 0 ]]; then
        echo "FAIL: sentinel '${SENTINEL}' not seen in pane within 15 s." >&2
        tmux capture-pane -t "$TMUX_SESS_CMD" -p -S - >&2
        exit 1
    fi
    echo "    PASS: remote command output received."

    # ── 4. Verify etr exits when the command finishes ─────────────────────────
    echo "    waiting for etr to exit after command completes..."
    EXIT_SEEN=0
    for i in $(seq 1 12); do
        sleep 1
        tmux has-session -t "$TMUX_SESS_CMD" 2>/dev/null || { EXIT_SEEN=1; break; }
    done
    if [[ $EXIT_SEEN -eq 0 ]]; then
        echo "FAIL: etr did not exit within 12 s after command should have finished." >&2
        exit 1
    fi
    echo "    PASS: etr exited cleanly when command finished."

    echo ""
    echo "==> Part 1 passed."

    # ── 5. Fast-exit: command exits before client connects ────────────────────
    # Regression test for the raw-mode hang: if the remote command exits
    # immediately (e.g. command not found), etr must exit cleanly rather than
    # spin in the reconnect loop with Ctrl-C disabled.
    > "$CLIENT_LOG"
    TMUX_SESS_FAST="etr_cmd_fast_test"
    echo "==> Part 2: fast-exit command ('true') must not hang..."
    tmux new-session -d -s "$TMUX_SESS_FAST" -x 200 -y 50 -- \
        "{{INSTALL}}/etr" -v localhost true

    FAST_EXIT=0
    for i in $(seq 1 20); do
        sleep 1
        tmux has-session -t "$TMUX_SESS_FAST" 2>/dev/null || { FAST_EXIT=1; break; }
    done
    if [[ $FAST_EXIT -eq 0 ]]; then
        echo "FAIL: etr did not exit within 20 s after 'true'; likely hung in reconnect loop." >&2
        tmux kill-session -t "$TMUX_SESS_FAST" 2>/dev/null || true
        exit 1
    fi
    echo "    PASS: etr exited cleanly after fast-exit command."

    # ── 6. Redirected stdin must not truncate the command's output ───────────
    # Regression test for the v0.7.5 fix. `etr host 'cmd' </dev/null` used to end
    # the session the instant stdin hit EOF -- `run_session`'s select! treated
    # stdin_task completing as session end -- so the output was discarded and etr
    # exited 0 having printed nothing but the terminal reset.
    #
    # tmux is deliberately NOT used here: the pane's stdin is the tmux pty, which
    # never EOFs, so a tmux-hosted run cannot exercise this path at all. The
    # command needs a controlling terminal (raw mode) AND a redirected stdin at
    # the same time, so it runs under `sh -c` inside a pty with fd 0 redirected.
    > "$CLIENT_LOG"
    SENTINEL_EOF="EOF_TEST_SENTINEL_$$"
    echo "==> Part 3: redirected stdin must not truncate output..."

    # Bare `mktemp` rather than `mktemp -t …`: the -t form means different things
    # in GNU and BSD coreutils, and macOS is a supported test platform.
    PTY_HELPER="$(mktemp)"
    OUT_FILE="$(mktemp)"
    cat > "$PTY_HELPER" <<'PYEOF'
    import os, pty, select, sys, time
    pid, mfd = pty.fork()
    if pid == 0:
        os.execv("/bin/sh", ["/bin/sh", "-c", sys.argv[1]])
    chunks, deadline = [], time.time() + 40
    while time.time() < deadline:
        r, _, _ = select.select([mfd], [], [], 1.0)
        if r:
            try:
                d = os.read(mfd, 4096)
            except OSError:
                break
            if not d:
                break
            chunks.append(d)
        if os.waitpid(pid, os.WNOHANG)[0] != 0:
            while True:
                rr, _, _ = select.select([mfd], [], [], 0.5)
                if not rr:
                    break
                try:
                    d = os.read(mfd, 4096)
                except OSError:
                    break
                if not d:
                    break
                chunks.append(d)
            break
    sys.stdout.buffer.write(b"".join(chunks))
    PYEOF

    "{{PY}}" "$PTY_HELPER" \
        "exec {{INSTALL}}/etr localhost 'echo ${SENTINEL_EOF}' </dev/null" > "$OUT_FILE" 2>&1 || true

    if ! grep -q "${SENTINEL_EOF}" "$OUT_FILE"; then
        echo "FAIL: output of a remote command was lost when stdin was redirected." >&2
        echo "      Expected '${SENTINEL_EOF}' in etr's output; got $(wc -c < "$OUT_FILE") bytes:" >&2
        cat -v "$OUT_FILE" >&2
        rm -f "$PTY_HELPER" "$OUT_FILE"
        exit 1
    fi
    echo "    PASS: remote command output survives stdin EOF."

    # A command that *reads* stdin must still terminate: a PTY cannot be
    # half-closed, so the client relays VEOF (0x04) on stdin EOF. Without it this
    # hangs until the server's reconnect timeout instead of failing fast.
    SENTINEL_CAT="CAT_TEST_SENTINEL_$$"
    echo "==> Part 4: a stdin-reading command must see EOF and exit..."
    START=$(date +%s)
    "{{PY}}" "$PTY_HELPER" \
        "exec sh -c 'printf \"${SENTINEL_CAT}\\n\" | {{INSTALL}}/etr localhost cat'" \
        > "$OUT_FILE" 2>&1 || true
    ELAPSED=$(( $(date +%s) - START ))

    if ! grep -q "${SENTINEL_CAT}" "$OUT_FILE"; then
        echo "FAIL: 'cat' did not echo piped stdin back (elapsed ${ELAPSED}s)." >&2
        cat -v "$OUT_FILE" >&2
        rm -f "$PTY_HELPER" "$OUT_FILE"
        exit 1
    fi
    if [[ $ELAPSED -ge 35 ]]; then
        echo "FAIL: 'cat' took ${ELAPSED}s — it never saw EOF and hung." >&2
        rm -f "$PTY_HELPER" "$OUT_FILE"
        exit 1
    fi
    echo "    PASS: stdin-reading command saw EOF and exited (${ELAPSED}s)."

    # ── 7. No controlling terminal ───────────────────────────────────────────
    # Regression test for the v0.7.6 fix. enable_raw_mode() opens /dev/tty, which
    # fails with ENXIO under cron, a systemd unit, CI or an agent shell; that call
    # was `.unwrap()`, so those contexts got a Rust panic and exit 101.
    #
    # `setsid(2)` via python rather than the setsid(1) binary: the command does not
    # exist on macOS, which is a supported platform for these tests.
    NOTTY_HELPER="$(mktemp)"
    cat > "$NOTTY_HELPER" <<'PYEOF'
    import os, sys
    os.setsid()                      # detach from the controlling terminal
    os.execv("/bin/sh", ["/bin/sh", "-c", sys.argv[1]])
    PYEOF

    SENTINEL_NOTTY="NOTTY_TEST_SENTINEL_$$"
    echo "==> Part 5: a remote command must run without a controlling terminal..."
    # `|| NOTTY_RC=$?` rather than a bare call: `set -e` is in force, so a failing
    # run would abort the recipe before the exit code could be inspected -- the
    # exit-101 check below would be unreachable and the test would fail silently,
    # reporting nothing about the panic it exists to catch.
    NOTTY_RC=0
    "{{PY}}" "$NOTTY_HELPER" \
        "exec {{INSTALL}}/etr localhost 'echo ${SENTINEL_NOTTY}' </dev/null" \
        > "$OUT_FILE" 2>/dev/null || NOTTY_RC=$?

    if [[ $NOTTY_RC -eq 101 ]]; then
        echo "FAIL: etr panicked (exit 101) with no controlling terminal." >&2
        rm -f "$PTY_HELPER" "$OUT_FILE" "$NOTTY_HELPER"
        exit 1
    fi
    if ! grep -q "${SENTINEL_NOTTY}" "$OUT_FILE"; then
        echo "FAIL: no command output with no controlling terminal (rc=${NOTTY_RC})." >&2
        cat -v "$OUT_FILE" >&2
        rm -f "$PTY_HELPER" "$OUT_FILE" "$NOTTY_HELPER"
        exit 1
    fi
    # The output must be byte-clean. With no terminal there is nothing to reset, so
    # restore_terminal must stay silent -- otherwise 70 bytes of VT escapes land in
    # what is really a file or a pipe, corrupting the output Part 3 exists to
    # deliver. Checking for ESC catches that directly.
    if LC_ALL=C grep -q $'\033' "$OUT_FILE"; then
        echo "FAIL: VT escape sequences leaked into non-terminal output." >&2
        cat -v "$OUT_FILE" >&2
        rm -f "$PTY_HELPER" "$OUT_FILE" "$NOTTY_HELPER"
        exit 1
    fi
    echo "    PASS: ran with no terminal, output present and free of escapes."

    echo "==> Part 6: an interactive session with no terminal must fail clearly..."
    # Same `set -e` guard as Part 5 -- and here a non-zero exit is the *expected*
    # result, so without it this check could never pass.
    INT_RC=0
    "{{PY}}" "$NOTTY_HELPER" "exec {{INSTALL}}/etr localhost </dev/null" \
        > "$OUT_FILE" 2>&1 || INT_RC=$?

    if [[ $INT_RC -eq 0 ]]; then
        echo "FAIL: interactive session with no terminal exited 0; expected a refusal." >&2
        rm -f "$PTY_HELPER" "$OUT_FILE" "$NOTTY_HELPER"
        exit 1
    fi
    if [[ $INT_RC -eq 101 ]]; then
        echo "FAIL: interactive session panicked (exit 101) instead of reporting." >&2
        cat -v "$OUT_FILE" >&2
        rm -f "$PTY_HELPER" "$OUT_FILE" "$NOTTY_HELPER"
        exit 1
    fi
    if ! grep -q 'no controlling terminal' "$OUT_FILE"; then
        echo "FAIL: refusal did not name the missing terminal (rc=${INT_RC}):" >&2
        cat -v "$OUT_FILE" >&2
        rm -f "$PTY_HELPER" "$OUT_FILE" "$NOTTY_HELPER"
        exit 1
    fi
    echo "    PASS: refused with a diagnostic naming the missing terminal (rc=${INT_RC})."

    # ── 8. Teardown must not wait for the remote command ─────────────────────
    # Regression test for the v0.7.8 fix. Two spawn_blocking tasks -- the PTY reader
    # and the child waiter -- only finish when the shell exits, and Runtime::drop
    # waits for blocking tasks. Worse, the reader holds a PTY master clone, so the
    # PTY could never hang up on its own: a self-sustaining deadlock. etrs therefore
    # outlived its own "shutting down" log line for as long as the command ran --
    # forever, for a long-running one. Interactive sessions were unaffected (a login
    # shell exits by itself), which is why this hid for so long.
    echo "==> Part 7: SIGTERM must tear the server down, not wait for the command..."
    # Identify THIS test's server by diffing the pid set, not `pgrep | head -1`:
    # picking an arbitrary etrs can signal a different session entirely and then
    # blame this one for the survivor. (That is exactly what the first version of
    # this check did -- it reported an orphaned command while the process it had
    # actually killed was somebody else's.)
    BEFORE_PIDS=" $(pgrep -x -u "$(id -u)" etrs 2>/dev/null | tr '\n' ' ') "
    "{{PY}}" "$NOTTY_HELPER" \
        "exec {{INSTALL}}/etr localhost 'sleep 300' </dev/null >/dev/null 2>&1 &
         sleep 6; exit 0" >/dev/null 2>&1 || true

    ETRS_TD=""
    for p in $(pgrep -x -u "$(id -u)" etrs 2>/dev/null || true); do
        [[ "$BEFORE_PIDS" == *" $p "* ]] || { ETRS_TD="$p"; break; }
    done
    if [[ -z "$ETRS_TD" ]]; then
        echo "SKIP: no new etrs to signal (session did not start)." >&2
    else
        # Its child is the remote command; track that pid rather than matching on
        # a command name, which would also catch unrelated sleeps.
        CMD_PID=$(pgrep -P "$ETRS_TD" 2>/dev/null | head -1 || true)
        TD_START=$(date +%s)
        kill -TERM "$ETRS_TD" 2>/dev/null || true
        TD_GONE=0
        for _ in $(seq 1 30); do
            kill -0 "$ETRS_TD" 2>/dev/null || { TD_GONE=1; break; }
            sleep 0.5
        done
        TD_ELAPSED=$(( $(date +%s) - TD_START ))
        if [[ $TD_GONE -eq 0 ]]; then
            echo "FAIL: etrs still alive ${TD_ELAPSED}s after SIGTERM -- teardown is" >&2
            echo "      blocked on the remote command (sleep 300) instead of exiting." >&2
            kill -9 "$ETRS_TD" 2>/dev/null || true
            rm -f "$PTY_HELPER" "$OUT_FILE" "$NOTTY_HELPER"
            exit 1
        fi
        echo "    PASS: etrs exited ${TD_ELAPSED}s after SIGTERM, with the command still running."
        # The command must not be left behind either -- checked by pid, and only if
        # we managed to identify it.
        if [[ -n "$CMD_PID" ]]; then
            CMD_GONE=0
            for _ in $(seq 1 10); do
                kill -0 "$CMD_PID" 2>/dev/null || { CMD_GONE=1; break; }
                sleep 0.5
            done
            if [[ $CMD_GONE -eq 0 ]]; then
                echo "FAIL: the remote command (pid ${CMD_PID}) outlived the server." >&2
                kill -9 "$CMD_PID" 2>/dev/null || true
                rm -f "$PTY_HELPER" "$OUT_FILE" "$NOTTY_HELPER"
                exit 1
            fi
            echo "    PASS: the remote command was hung up with the session."
        fi
    fi
    rm -f "$PTY_HELPER" "$OUT_FILE" "$NOTTY_HELPER"

    echo ""
    echo "==> All remote command tests passed."

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

# Regression test: concurrent UDP senders through -L (v0.4.9 per-sender routing fix)
#
# Before v0.4.9 the server used a single "last_peer" socket so replies from the
# remote UDP target always went to whichever sender sent last (last-sender-wins).
# This test sends interleaved datagrams from two independent sockets and asserts
# each socket receives its own reply — not the other sender's.
e2e-udp-concurrent: check-tools install
    #!/usr/bin/env bash
    set -euo pipefail

    CLIENT_LOG="${XDG_STATE_HOME:-$HOME/.local/state}/etr/etr.log"
    TMUX_CONC="etr_udp_concurrent"
    UDP_ECHO_PORT=19341
    UDP_FWD_PORT=19342

    cleanup() {
        echo ""
        echo "--- cleanup ---"
        kill "${UDP_ECHO_PID:-}" 2>/dev/null || true
        tmux kill-session -t "$TMUX_CONC" 2>/dev/null || true
        pkill -x etrs 2>/dev/null || true
    }
    trap cleanup EXIT

    mkdir -p "$(dirname "$CLIENT_LOG")"
    > "$CLIENT_LOG"

    echo "==> Starting UDP echo server on port ${UDP_ECHO_PORT}..."
    python3 "{{justfile_directory()}}/scripts/stress/udp_echo.py" "${UDP_ECHO_PORT}" &
    UDP_ECHO_PID=$!
    sleep 0.3

    echo "==> Launching etr with -L UDP spec..."
    tmux new-session -d -s "$TMUX_CONC" -x 200 -y 50 -- \
        "{{INSTALL}}/etr" -v \
        -L "${UDP_FWD_PORT}:127.0.0.1:${UDP_ECHO_PORT}/udp" \
        localhost

    echo "    waiting for etr to connect..."
    READY=0
    for i in $(seq 1 30); do
        sleep 1
        grep -q '\[etr\] Connected\.' "$CLIENT_LOG" 2>/dev/null && { READY=1; break; }
    done
    if [[ $READY -eq 0 ]]; then
        echo "ERROR: '[etr] Connected.' not seen within 30 s" >&2
        cat "$CLIENT_LOG" >&2
        exit 1
    fi
    sleep 1.0  # allow -L listener to bind

    # ── Two senders, interleaved, each must get back its own payload ──────────
    echo "==> Testing concurrent UDP senders (regression for v0.4.9 routing fix)..."
    RESULT=$(python3 "{{justfile_directory()}}/scripts/stress/udp_concurrent_senders.py" "${UDP_FWD_PORT}" 2>&1 || true)
    if [[ "$RESULT" == "PASS" ]]; then
        echo "    PASS: each concurrent sender received its own reply."
    else
        echo "FAIL: concurrent UDP sender routing incorrect." >&2
        echo "--- result ---" >&2
        echo "$RESULT" >&2
        exit 1
    fi

    echo ""
    echo "==> Concurrent UDP sender regression test passed."

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
        pkill -x stress_tool 2>/dev/null || true
        tmux kill-session -t "$STRESS_SESS" 2>/dev/null || true
        pkill -x etrs 2>/dev/null || true
        rm -f "$TCP_PUMP_OUT" "$UDP_PUMP_OUT" "$TCP_R_PUMP_OUT" "$UDP_R_PUMP_OUT"
    }
    trap cleanup EXIT

    # Kill any stress_tool processes left over from a previous crashed run so
    # their ports are free before we try to bind them.
    pkill -x stress_tool 2>/dev/null || true
    sleep 0.3

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

    # Helper: block until a TCP port is accepting connections (Linux + macOS bash).
    # /dev/tcp is a bash built-in — no external tools required.
    wait_tcp_ready() {
        local port=$1 label=$2
        local deadline=$(( SECONDS + 15 ))
        while (( SECONDS < deadline )); do
            bash -c "echo >/dev/tcp/127.0.0.1/${port}" 2>/dev/null && return 0
            sleep 0.2
        done
        echo "ERROR: ${label} (127.0.0.1:${port}) not ready after 15s" >&2
        exit 1
    }

    # ── -L pumps ──────────────────────────────────────────────────────────────
    echo "==> TCP -L pump on :${TCP_FWD_PORT}..."
    wait_tcp_ready "$TCP_FWD_PORT" "TCP -L forward listener"
    "$STRESS_BIN" tcp-pump "$TCP_FWD_PORT" > "$TCP_PUMP_OUT" &
    TCP_PUMP_PID=$!

    echo "==> UDP -L pump on :${UDP_FWD_PORT}..."
    "$STRESS_BIN" udp-pump "$UDP_FWD_PORT" > "$UDP_PUMP_OUT" &
    UDP_PUMP_PID=$!

    # ── -R pumps (etrs binds these after the QUIC session is fully up) ────────
    # Probe the TCP -R port; once it accepts, the UDP -R listener is also ready.
    echo "==> TCP -R pump on :${TCP_R_FWD_PORT}..."
    wait_tcp_ready "$TCP_R_FWD_PORT" "TCP -R forward listener"
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

# Remove build artifacts (see clean-procs for leftover test processes)
clean:
    #!/usr/bin/env bash
    set -euo pipefail
    # Build artifacts ONLY. This deliberately does not touch processes: it used to
    # start with `pkill -x etrs`, which kills EVERY etrs the user owns -- including
    # the server hosting a live remote session on this machine. Someone clearing
    # build output has not asked for their sessions to be dropped, and a plain
    # `cargo clean` gives no hint that it might. Process cleanup now lives in
    # `just clean-procs`, where it is the stated purpose and is confirmed first.
    cargo clean
    # cargo clean only knows about this crate's target dir. These are gitignored
    # build/dev output it leaves behind, so "clean" never actually cleaned them:
    rm -rf man/build                 # rebuilt by `just man`
    rm -rf tools/stress/target       # a separate crate, invisible to the root clean
    rm -f etrs_fwd.json.gz           # profile capture; AGENTS.md 4.10 wants it gone before tagging
    # Plain `echo`, not `@echo`: the `@` line-suppression prefix is just's syntax for
    # NON-shebang recipes. Inside a `#!/usr/bin/env bash` recipe it is handed to bash
    # verbatim, which fails with `@echo: command not found` and exit 127 -- after the
    # cleaning has already happened, so it looks like clean broke rather than that its
    # last line did.
    echo "cleaned build artifacts (processes untouched -- see: just clean-procs)"

# Kill leftover test/dev processes and tmux sessions, after showing what they are
clean-procs:
    #!/usr/bin/env bash
    set -euo pipefail
    RED=$'\033[0;31m'; GREEN=$'\033[0;32m'; YELLOW=$'\033[1;33m'; NC=$'\033[0m'

    # `pgrep -x` (exact process name), never `pgrep -f`: a full-command-line match
    # would also match this very recipe's own shell, whose command line contains the
    # pattern -- the classic self-match that makes a filter kill its own script.
    # Restricted to the invoking user so a shared box's other users are never touched.
    ETRS_PIDS=$(pgrep -x -u "$(id -u)" etrs 2>/dev/null || true)
    STRESS_PIDS=$(pgrep -x -u "$(id -u)" stress_tool 2>/dev/null || true)
    # Every test session name in this justfile starts with etr_ (etr_test,
    # etr_cmd_test, etr_env_test, etr_forward_test, etr_reverse_test,
    # etr_udp_concurrent, etr_stress...), so one prefix covers them all.
    SESSIONS=$(tmux list-sessions -F '#{session_name}' 2>/dev/null | grep '^etr_' || true)

    if [ -z "$ETRS_PIDS" ] && [ -z "$STRESS_PIDS" ] && [ -z "$SESSIONS" ]; then
        echo "${GREEN}Nothing to clean up.${NC} No etrs or stress_tool processes, no etr_* tmux sessions."
        exit 0
    fi

    # Show them BEFORE killing anything. This is the whole point of the recipe: an
    # orphaned test server and the server hosting a live remote session are
    # indistinguishable to pkill, but not to a human reading start times and args.
    echo "${YELLOW}Found the following. Some may be REAL sessions, not leftovers:${NC}"
    echo
    if [ -n "$ETRS_PIDS" ]; then
        echo "  etrs processes (each one is somebody's session -- including yours):"
        # shellcheck disable=SC2086
        ps -o pid=,lstart=,etime=,args= -p $(echo "$ETRS_PIDS" | tr '\n' ' ') | sed 's/^/    /'
        echo
    fi
    if [ -n "$STRESS_PIDS" ]; then
        echo "  stress_tool processes:"
        # shellcheck disable=SC2086
        ps -o pid=,etime=,args= -p $(echo "$STRESS_PIDS" | tr '\n' ' ') | sed 's/^/    /'
        echo
    fi
    if [ -n "$SESSIONS" ]; then
        echo "  tmux sessions matching etr_*:"
        echo "$SESSIONS" | sed 's/^/    /'
        echo
    fi

    # Same three answer sources as `just pr`, for the same reason: a cleanup step is
    # often run from a script, and a bare `read` there blocks on a stdin nobody holds.
    if [ -n "${CLEAN_CONFIRM:-}" ]; then
        CONFIRM="$CLEAN_CONFIRM"
        echo "Kill all of the above? [y/N] $CONFIRM   (answered by CLEAN_CONFIRM)"
    elif [ -t 0 ]; then
        echo -n "Kill all of the above? [y/N] "
        read -r CONFIRM
    else
        echo -n "Kill all of the above? [y/N] "
        read -r -t 10 CONFIRM || CONFIRM=""
        echo "$CONFIRM"
        [ -n "$CONFIRM" ] || { echo "${RED}Aborted.${NC} No terminal to confirm on and nothing on stdin. Re-run with CLEAN_CONFIRM=y."; exit 1; }
    fi
    [ "$CONFIRM" = "y" ] || [ "$CONFIRM" = "Y" ] \
        || { echo "${RED}Aborted.${NC} Nothing was killed."; exit 1; }

    for s in $SESSIONS; do
        tmux kill-session -t "$s" 2>/dev/null && echo "  killed tmux session $s" || true
    done

    # TERM first, then escalate. An orphaned etrs does not always go on SIGTERM --
    # observed needing SIGKILL -- and a reap that leaves the process running while
    # reporting success is worse than one that never ran.
    ALL_PIDS=$(printf '%s\n%s\n' "$ETRS_PIDS" "$STRESS_PIDS" | grep -v '^$' || true)
    [ -n "$ALL_PIDS" ] || { echo "${GREEN}Done.${NC}"; exit 0; }

    while read -r pid; do
        kill -TERM "$pid" 2>/dev/null || true
    done <<< "$ALL_PIDS"

    # Give them a moment to exit cleanly (etrs records a utmp logout on the way out).
    for _ in 1 2 3 4 5 6; do
        REMAINING=""
        while read -r pid; do
            kill -0 "$pid" 2>/dev/null && REMAINING="$REMAINING $pid"
        done <<< "$ALL_PIDS"
        [ -n "$REMAINING" ] || break
        sleep 0.5
    done

    if [ -n "${REMAINING:-}" ]; then
        echo "${YELLOW}Still running after SIGTERM:${NC}$REMAINING -- sending SIGKILL"
        for pid in $REMAINING; do
            kill -9 "$pid" 2>/dev/null || true
        done
        sleep 0.5
        STUBBORN=""
        for pid in $REMAINING; do
            kill -0 "$pid" 2>/dev/null && STUBBORN="$STUBBORN $pid"
        done
        [ -z "$STUBBORN" ] || { echo "${RED}FAILED to kill:${NC}$STUBBORN"; exit 1; }
    fi
    echo "${GREEN}Done.${NC} All listed processes and sessions are gone."
