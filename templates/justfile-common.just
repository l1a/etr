# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 l1a
#
# ============================================================================
# CANONICAL COMMON JUSTFILE BLOCK — template v3
# ============================================================================
#
# The text below `---- BEGIN CANONICAL ----` is the reference for the block
# delimited by `# >>> COMMON (template v3)` and `# <<< COMMON` in a project's
# Justfile. Vendored in: rusticprofile, retch, etr.
#
# It is a THIN layer. The real work lives in two vendored Python helpers,
# `scripts/install_completions.py` and `scripts/install_man.py`, which carry
# their own rationale and their own `--self-test`.
#
#
# WHY PYTHON HELPERS RATHER THAN JUST RECIPES  (v2 reversed v1 here)
# -----------------------------------------------------------------
# This is retch's finding, adopted as the standard. retch converted these exact
# recipes to Python in its v0.6.16 to eliminate bash-shebang escaping bugs and
# run natively on Windows PowerShell, CMD and Unix shells WITHOUT requiring
# Git's `usrin` on PATH.
#
# v1 of this template got that backwards: it took rusticprofile's plain-`sh`
# recipes as canonical because rusticprofile had the CORRECTNESS fixes, and
# would have regressed retch's deliberate portability work in the name of
# consistency. The two repos had each solved half the problem. v2 keeps retch's
# mechanism and rusticprofile's measured correctness:
#
#   * a `bash` shebang recipe cannot run on Windows without `cygpath` at all
#   * a plain `sh` recipe still needs an `sh` on PATH
#   * a Python helper needs only an interpreter, and its logic is unit-testable
#     off-platform — which matters because the branch that keeps being wrong is
#     the Windows one, on machines that never take it
#
# The lesson worth keeping: "the repo with the fixes" and "the repo with the
# right mechanism" were not the same repo, and picking a reference without
# reading each one's release log would have shipped a regression as a standard.
#
#
# WHY A VENDORED COPY RATHER THAN `just import`
# ---------------------------------------------
# Three separate git repositories. An `import` still needs the imported file to
# exist in each one, so it moves the duplication rather than removing it, and a
# submodule for one file costs more than it saves. What makes a copy safe is not
# that it is a copy — it is that something CHECKS it.
#
# Not hypothetical. rusticprofile measured and fixed three completion defects and
# both siblings then carried them for months, because nothing compared them:
#
#   1. nushell completions written to the XDG path, which Windows nushell never
#      reads — live in retch AND etr
#   2. output claiming zsh completions are "auto-loaded from the default $fpath",
#      false on every distribution — live in etr
#   3. `install-completions` as a bash shebang recipe, unrunnable on Windows — etr
#
# And retch has a fourth of its own: its helper logs a generation failure to
# stderr, carries on, and then prints "Installed completions for retch:" with
# the full path list regardless — success reported over work not done.
#
#
# WHY `standard-check` RUNS SELF-TESTS RATHER THAN DIFFING TEXT
# ------------------------------------------------------------
# With the logic in a script, a within-repo text diff checks nothing useful, and
# across repos it is impossible — git cannot diff a file it does not have. So
# each helper asserts its own invariants and `standard-check` runs them. That is
# the stronger property anyway: it is what was actually violated above, and a
# text diff would pass happily on a repo that never adopted the standard at all.
#
#
# WHY EVERY RECIPE IS WRITTEN AGAINST VARIABLES
# ---------------------------------------------
# `etr` ships two binaries (`etr`, `etrs`); the others ship one. A block with a
# hardcoded binary name cannot be copied, and a standard that cannot be copied is
# decoration. Project facts live ABOVE the block, in a PROJECT header each repo
# owns:
#
#     BINS      := "etr etrs"                          # space separated
#     MAN_PAGES := "man/build/etr.1 man/build/etrs.1"  # space separated
#
# Project-specific recipes are normal — put them OUTSIDE the markers, under
# `# ===== PROJECT-SPECIFIC =====`. Editing INSIDE the markers means changing the
# standard: edit this file and both helpers, bump TEMPLATE_VERSION and the marker,
# and propagate to the siblings in their own PRs.
#
#
# SCOPE, STATED SO THE GAPS ARE NOT READ AS OVERSIGHTS
# ----------------------------------------------------
# Covers the install/man/completions family, and — since v3 — the BEHAVIOUR of the
# `pr`/`open-pr`/`merge-pr` triad, via `scripts/gate_conformance.py`.
#
# The triad's *bodies* are still not shared, and that is deliberate: `cargo clippy` is
# bare in rusticprofile, `--workspace` in retch and `--all-targets` in etr; the NOTES
# header differs; each checklist asks for different things. Forcing one body would
# change what each gate DOES. So what is standardised is the set of guards each recipe
# must carry, and `gate_conformance.py` asserts they are present.
#
# That check is STRUCTURAL, not behavioural, and its own docstring says so: it proves a
# guard exists, not that it works. The install helpers can be checked behaviourally
# because they are pure functions; the gate recipes run test suites, push branches and
# merge PRs, so executing them from `check` would be slow and destructive.
#
# Still not covered: `check`/`lint`/`test` bodies, and `man` — one repo commits its
# page, one gitignores it, one builds two.
#
# Known divergences left alone, for whoever extends this:
#   * `Justfile` (rusticprofile, retch) vs `justfile` (etr)
#   * `lint` (rusticprofile, retch) vs `clippy` (etr)
#   * `## Current State (vX)` vs `## Current state: vX` in NOTES.md
#   * etr has no `install-hooks` and no `open-pr`, so its pre-push gate is
#     installed by nothing and its PR path has no gated call site
#
# ---- BEGIN CANONICAL ----
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
