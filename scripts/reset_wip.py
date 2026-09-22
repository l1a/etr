#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Reset WIP.md after merging a feature branch to main, and guard its line endings.

WIP.md is the gitignored, Syncthing-synced cross-machine handoff file (`AGENTS.md` Part 1
section 3). Two things follow from "gitignored" that make this script carry more than a
couple of regex substitutions.

WHY THE LINE-ENDING GUARD LIVES HERE. `.gitattributes` pins `* text=auto eol=lf` for the
whole tree, and `scripts/text_check.py` refuses a carriage return in tracked text -- but
BOTH work through git, and `git ls-files` never offers a gitignored path. WIP.md is
therefore the one text file in this repository with no automatic protection, and it is
rewritten on every `just merge-pr` by this script. LF is the base model for every non-binary
file here (the tree is Linux-primary; measured 2026-09-22: 71 tracked text files, 0 carriage
returns, and WIP.md itself 0), so the file wants the same guarantee the rest of the tree
gets. `--check-endings` supplies it, and `just check` runs it via `just wip-check`.

WHY THE REWRITE PRESERVES THE TERMINATOR RATHER THAN HARDCODING LF. Reading with
`read_text()` and writing with `write_text()` silently translates: a CRLF file read on Linux
comes back with `\\n` and goes out as LF. That converts a file as a SIDE EFFECT OF A MERGE,
which is a thing to be caught, not a thing to do quietly -- the sibling repo `retch` shipped
exactly that accident in its v0.17.12, in the other direction. So this script reads and
writes BYTES, re-applying whatever terminator it found, and the decision that the terminator
should be LF is asserted by `--check-endings` where it is visible, rather than falling out of
an I/O default where it is not.

WHY IT REFUSES ON A MATCH COUNT OTHER THAN ONE. Both substitutions used to be unbounded
`re.sub` over the whole file. While WIP.md was a rolling session log that meant every
historical `### Active Branch:` heading was rewritten on every merge, and -- worse -- the
`**Latest commit on main**` pattern matched NOTHING at all, so the script printed "Latest
commit updated to ..." having updated nothing. A rewrite that reports success without
finding its target is the failure mode `retch` hit from the other end (its #259, where both
patterns matched stale entries far down the file). WIP.md now carries exactly one state
block; requiring exactly one match of each pattern is what keeps that true.
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

TEMPLATE_VERSION = 1

# Anchored to line start, and that is load-bearing rather than tidy: WIP.md's own header
# names both markers in prose ("it rewrites the `### Active Branch:` line"), and an unanchored
# pattern matches those mentions too -- which is how the exactly-one rule first fired.
ACTIVE_BRANCH_RE = re.compile(rb"(?m)^### Active Branch:.*")
LATEST_COMMIT_RE = re.compile(rb"(?m)^\*\*Latest commit on main\*\*:.*")


def run(cmd):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, check=True).stdout.strip()
    except subprocess.CalledProcessError as e:
        print(f"Command failed: {' '.join(cmd)}", file=sys.stderr)
        if e.stdout:
            print(e.stdout, file=sys.stderr)
        if e.stderr:
            print(e.stderr, file=sys.stderr)
        sys.exit(e.returncode)


def substitute_once(pattern, replacement, data, label):
    """Replace the single occurrence of `pattern`, or raise naming the real count.

    Returns the new bytes. Raises ValueError on 0 or 2+ matches -- the caller writes
    nothing in that case, so a WIP.md whose shape has drifted fails loudly instead of
    being silently half-rewritten.
    """
    count = len(pattern.findall(data))
    if count != 1:
        raise ValueError(
            f"WIP.md must contain exactly one {label} line; found {count}. "
            f"WIP.md carries one state block (see its own header); "
            f"reconcile it before re-running."
        )
    return pattern.sub(replacement, data, count=1)


def detect_terminator(data):
    """Return the terminator to write back: CRLF only if the file is predominantly CRLF."""
    crlf = data.count(b"\r\n")
    lf = data.count(b"\n") - crlf
    return b"\r\n" if crlf > lf else b"\n"


def reset(wip_file, commit_hash, commit_msg):
    """Rewrite the state block in `wip_file`, preserving its byte-level line endings."""
    raw = wip_file.read_bytes()
    terminator = detect_terminator(raw)
    data = raw.replace(b"\r\n", b"\n")

    data = substitute_once(
        ACTIVE_BRANCH_RE,
        b"### Active Branch: none (main is current)",
        data,
        "`### Active Branch:`",
    )
    data = substitute_once(
        LATEST_COMMIT_RE,
        f"**Latest commit on main**: {commit_hash} ({commit_msg})".encode(),
        data,
        "`**Latest commit on main**:`",
    )

    if terminator != b"\n":
        data = data.replace(b"\n", b"\r\n")
    wip_file.write_bytes(data)


def check_endings(wip_file):
    """Fail if WIP.md holds any carriage return. An absent file passes.

    Absent is not a failure: WIP.md is per-machine and untracked, so a fresh clone and CI
    both legitimately have none. Counting BYTES rather than reaching for `grep -c` is
    deliberate -- see `~/AGENTS.md` section 17, where that idiom returns the file's line
    count wearing a carriage-return costume.
    """
    if not wip_file.exists():
        print("WIP.md absent (per-machine, untracked) -- nothing to check")
        return 0
    data = wip_file.read_bytes()
    cr = data.count(b"\r")
    if cr:
        print(
            f"WIP.md contains {cr} carriage return(s); this tree is LF.\n"
            f"  It is gitignored, so .gitattributes and text-check cannot reach it.\n"
            f"  Fix: python3 -c \"import pathlib;p=pathlib.Path('WIP.md');"
            f"p.write_bytes(p.read_bytes().replace(b'\\r\\n',b'\\n').replace(b'\\r',b''))\"",
            file=sys.stderr,
        )
        return 1
    print(f"WIP.md is LF ({data.count(b'\n')} lines)")
    return 0


def self_test():
    """Watch each refusal fire, and prove the round trip preserves bytes."""
    import contextlib
    import io
    import tempfile

    failures = []

    def quietly(fn, *a):
        """Run a guard with its output captured -- a control firing is not a failure here,
        and printing its refusal makes a passing self-test read like a failing one."""
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return fn(*a)

    def check(name, got, want):
        if got != want:
            failures.append(f"{name} -- expected {want!r}, got {got!r}")

    body_lf = (
        b"# WIP.md\n\n### Active Branch: feature/x\n"
        b"**Latest commit on main**: deadbee (old subject)\n\nnotes\n"
    )

    with tempfile.TemporaryDirectory() as td:
        d = Path(td)

        # 1. LF file round-trips as LF, and both lines are rewritten.
        f = d / "lf.md"
        f.write_bytes(body_lf)
        reset(f, "abc1234", "New subject")
        out = f.read_bytes()
        check("LF file stays LF", out.count(b"\r"), 0)
        check("active branch rewritten", b"### Active Branch: none (main is current)" in out, True)
        check("latest commit rewritten", b"**Latest commit on main**: abc1234 (New subject)" in out, True)

        # 2. A CRLF file is NOT converted -- conversion must never be a merge side effect.
        f = d / "crlf.md"
        f.write_bytes(body_lf.replace(b"\n", b"\r\n"))
        reset(f, "abc1234", "New subject")
        out = f.read_bytes()
        check("CRLF file stays CRLF", out.count(b"\r\n"), out.count(b"\n"))
        check("CRLF file still rewritten", b"### Active Branch: none (main is current)\r\n" in out, True)

        # 3. ...but --check-endings refuses it, which is where the decision lives.
        check("check-endings refuses CRLF", quietly(check_endings, f), 1)
        f2 = d / "ok.md"
        f2.write_bytes(body_lf)
        check("check-endings accepts LF", quietly(check_endings, f2), 0)
        check("check-endings accepts absent", quietly(check_endings, d / "nope.md"), 0)

        # 4. An in-prose mention of a marker must NOT count as a match. WIP.md's own header
        #    names both markers, which is how the exactly-one rule first fired.
        f = d / "prose.md"
        f.write_bytes(
            b"# WIP.md\n\nIt rewrites the `### Active Branch:` and "
            b"`**Latest commit on main**:` lines.\n\n" + body_lf
        )
        reset(f, "abc1234", "New subject")
        out = f.read_bytes()
        check("prose mention preserved", b"It rewrites the `### Active Branch:` and" in out, True)
        check("real heading rewritten", b"\n### Active Branch: none (main is current)\n" in out, True)

        # 5. Zero matches and duplicate matches both refuse, WRITING NOTHING.
        for name, content in (
            ("zero matches", body_lf.replace(b"**Latest commit on main**", b"**Latest commit**")),
            ("duplicate matches", body_lf + b"\n### Active Branch: stale\n"),
        ):
            f = d / "bad.md"
            f.write_bytes(content)
            try:
                reset(f, "abc1234", "New subject")
                check(f"{name} refused", "no refusal", "ValueError")
            except ValueError:
                pass
            check(f"{name} wrote nothing", f.read_bytes(), content)

    if failures:
        for line in failures:
            print(f"FAIL: {line}", file=sys.stderr)
        return 1
    print(f"reset_wip.py self-test passed (template v{TEMPLATE_VERSION})")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--self-test", action="store_true", help="run the guard's own negative controls")
    parser.add_argument("--check-endings", action="store_true", help="fail if WIP.md holds a carriage return")
    args = parser.parse_args()

    root_dir = Path(__file__).resolve().parent.parent
    wip_file = root_dir / "WIP.md"

    if args.self_test:
        sys.exit(self_test())
    if args.check_endings:
        sys.exit(check_endings(wip_file))

    if not wip_file.exists():
        print("WIP.md not found. Skipping reset.", file=sys.stderr)
        return

    commit_hash = run(["git", "rev-parse", "--short", "HEAD"])
    commit_msg = run(["git", "log", "-1", "--format=%s"])

    try:
        reset(wip_file, commit_hash, commit_msg)
    except ValueError as e:
        print(f"reset_wip.py: {e}", file=sys.stderr)
        sys.exit(1)
    print(f"Updated WIP.md: Active Branch set to none, Latest commit updated to {commit_hash} ({commit_msg})")


if __name__ == "__main__":
    main()
