#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 l1a
"""Install this repo's tracked git hooks into .git/hooks.

Python rather than a shell recipe for the same reason the install helpers are: a `bash`
shebang recipe cannot run on Windows without `cygpath`, and a plain `sh` recipe still needs
an `sh` on PATH. This needs only an interpreter.

**Why the hooks are tracked at all.** Until now etr had no `scripts/hooks/` directory: a
`pre-push` hook existed in one machine's `.git/hooks/`, untracked, so it was unreproducible
and no fresh clone got a gate. `AGENTS.md` Part 1 §4 leans on real git hooks as *the*
agent-agnostic enforcement layer, which only holds if the hook is in the repository.

Existing hooks are backed up rather than clobbered — someone may have a local hook that is
not ours, and silently replacing it would be the kind of quiet destruction these projects
refuse.
"""

import shutil
import stat
import subprocess
import sys
from pathlib import Path


def main():
    repo = Path(__file__).resolve().parent.parent
    src = repo / "scripts" / "hooks"
    if not src.is_dir():
        print(f"error: {src} does not exist", file=sys.stderr)
        return 1

    # Ask git where the hooks live rather than assuming `.git/hooks` — it is elsewhere in a
    # worktree or a submodule, and guessing would install into a directory git never reads.
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--git-path", "hooks"],
            cwd=repo, capture_output=True, text=True, check=True,
        )
    except (OSError, subprocess.CalledProcessError) as e:
        print(f"error: could not ask git for the hooks directory: {e}", file=sys.stderr)
        return 1
    dest = (repo / out.stdout.strip()).resolve()
    dest.mkdir(parents=True, exist_ok=True)

    installed = 0
    for hook in sorted(p for p in src.iterdir() if p.is_file()):
        target = dest / hook.name
        if target.exists() and target.read_bytes() != hook.read_bytes():
            backup = target.with_suffix(target.suffix + ".bak")
            shutil.copyfile(target, backup)
            print(f"  backed up existing {hook.name} -> {backup.name}")
        shutil.copyfile(hook, target)
        target.chmod(target.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
        print(f"  installed {hook.name}")
        installed += 1

    if not installed:
        print(f"error: no hooks found in {src}", file=sys.stderr)
        return 1
    print(f"{installed} hook(s) installed into {dest}")
    print("  skip once with: GIT_NO_CHECK=1 git push")
    return 0


if __name__ == "__main__":
    sys.exit(main())
