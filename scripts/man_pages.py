#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 l1a
"""Build and check the tracked man pages (man/etr.1, man/etrs.1) without a POSIX shell.

TEMPLATE v2 — run by `just man` (`--build`) and `just man-check` (`--self-test` + `--check`).

WHY THIS IS PYTHON RATHER THAN A BASH SHEBANG RECIPE
----------------------------------------------------
On Windows, `just` translates every `#!` recipe's temporary script path through `cygpath` before
invoking its interpreter, and in a Linux sandbox with a read-only `$XDG_RUNTIME_DIR` it cannot
create that temporary script at all. Moving `man` and `man-check` into a plain-recipe Python
helper (alongside `packaging-check` and `text-check`) makes all seven `just check` recipes and
`just man` completely shell-free.

WHAT `--check` PROVES (TWO INDEPENDENT TREES)
---------------------------------------------
1. WORKTREE: `man/etr.1` and `man/etrs.1` on disk match a fresh render of `man/etr.1.md` and
   `man/etrs.1.md` at the worktree `Cargo.toml` version, compared as raw bytes (so CRLF drift on
   disk fails just as a stale `.TH` version or an unrendered `.md` edit does).
2. HEAD: `HEAD:man/etr.1` and `HEAD:man/etrs.1` match a fresh render of `HEAD:man/etr.1.md` and
   `HEAD:man/etrs.1.md` at `HEAD:Cargo.toml`'s version. Check 1 alone cannot see a commit that
   disagrees with itself (#81: an `--amend` that staged `Cargo.toml` at 0.10.6 without staging
   `man/`, while the worktree copies on disk were current). `HEAD` is compared only with ITSELF,
   so uncommitted work in progress never trips it.
"""

from __future__ import annotations

import argparse
import difflib
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Callable

TEMPLATE_VERSION = 2

# Every repository-scoped variable listed by `git rev-parse --local-env-vars`. Inherited from a
# git hook (`pre-push`, `pre-commit`), a linked worktree, or `git -c ...` in a parent process,
# any of these will redirect a `cwd=repo_root` git call away from `repo_root` and onto the
# caller's repository or config.
_GIT_LOCAL_ENV_VARS: tuple[str, ...] = (
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_GRAFT_FILE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
)

REPO_ROOT = Path(__file__).resolve().parent.parent

# (markdown source relative to repo root, man title, rendered roff relative to repo root)
PAGES: tuple[tuple[Path, str, Path], ...] = (
    (Path("man/etr.1.md"), "ETR", Path("man/etr.1")),
    (Path("man/etrs.1.md"), "ETRS", Path("man/etrs.1")),
)

Renderer = Callable[[Path, str, str], bytes]


class ManPageError(Exception):
    """Raised when a man page cannot be built or parsed."""


def parse_cargo_version(cargo_toml_text: str) -> str:
    """Extract the crate version from Cargo.toml text."""
    for line in cargo_toml_text.splitlines():
        m = re.match(r'^version\s*=\s*"([^"]+)"', line.strip())
        if m:
            return m.group(1)
    raise ManPageError("could not find `version = \"...\"` in Cargo.toml")


def expected_th_header(name: str, version: str) -> bytes:
    """Return the canonical line-1 `.TH` header for `name` and `version`."""
    return f'.TH "{name}" "1" "" "etr {version}" "User Commands"'.encode("utf-8")


def rewrite_th_header(raw_mandown_output: bytes, name: str, version: str) -> bytes:
    """Normalise mandown output to LF and replace line 1 with the canonical .TH header."""
    lf_bytes = raw_mandown_output.replace(b"\r\n", b"\n")
    header = expected_th_header(name, version)
    _, sep, rest = lf_bytes.partition(b"\n")
    if not sep:
        return header + b"\n"
    return header + b"\n" + rest


def render_with_mandown(md_path: Path, name: str, version: str) -> bytes:
    """Render `md_path` via the `mandown` binary and rewrite its `.TH` header."""
    proc = subprocess.run(
        ["mandown", str(md_path), name, "1"],
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        stderr = proc.stderr.decode("utf-8", errors="replace").strip()
        raise ManPageError(f"mandown failed on {md_path} (exit {proc.returncode}): {stderr}")
    return rewrite_th_header(proc.stdout, name, version)


def _unified_diff_preview(actual: bytes, expected: bytes, fromfile: str, tofile: str) -> str:
    actual_lines = actual.decode("utf-8", errors="replace").splitlines(keepends=True)
    expected_lines = expected.decode("utf-8", errors="replace").splitlines(keepends=True)
    diff = list(difflib.unified_diff(actual_lines, expected_lines, fromfile=fromfile, tofile=tofile))
    return "".join(diff[:20]).rstrip("\n")


def build_pages(
    repo_root: Path = REPO_ROOT,
    renderer: Renderer | None = None,
    mandown_lookup: Callable[[str], str | None] = shutil.which,
) -> tuple[int, str]:
    """Render `man/etr.1` and `man/etrs.1` into `repo_root`. Returns (exit_code, message)."""
    if renderer is None:
        if mandown_lookup("mandown") is None:
            return 1, "ERROR: mandown is required to build man pages (cargo install mandown)"
        renderer = render_with_mandown

    cargo_toml = repo_root / "Cargo.toml"
    version = parse_cargo_version(cargo_toml.read_text(encoding="utf-8"))
    for md_rel, name, out_rel in PAGES:
        rendered = renderer(repo_root / md_rel, name, version)
        (repo_root / out_rel).write_bytes(rendered)
    return 0, f"Built man/etr.1 and man/etrs.1 (version {version})"


def _git_env(repo_root: Path, *, isolate_config: bool = False) -> dict[str, str]:
    """Return a clean environment where `repo_root` is the only git repository in scope.

    Stripping `_GIT_LOCAL_ENV_VARS` prevents an inherited `GIT_DIR` or `GIT_INDEX_FILE` (e.g.
    when `just check` is invoked from `scripts/hooks/pre-push` or a linked worktree) from
    redirecting `git` calls away from `repo_root`. When `isolate_config=True` (used for
    `--self-test` fixtures), system and user git configs (`commit.gpgsign`, `core.hooksPath`,
    etc.) are also disconnected so throwaway fixture commits never sign, prompt, or run user
    hooks.
    """
    env = {
        k: v
        for k, v in os.environ.items()
        if k not in _GIT_LOCAL_ENV_VARS
        and not k.startswith(("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"))
    }
    env["GIT_TERMINAL_PROMPT"] = "0"
    if isolate_config:
        for k in list(env):
            if k.startswith(("GIT_AUTHOR_", "GIT_COMMITTER_")):
                del env[k]
        env["GIT_CONFIG_NOSYSTEM"] = "1"
        env["GIT_CONFIG_GLOBAL"] = os.devnull
        env["HOME"] = str(repo_root)
        env["XDG_CONFIG_HOME"] = str(repo_root)
    return env


def _git_has_head(repo_root: Path) -> bool:
    proc = subprocess.run(
        ["git", "-C", str(repo_root), "rev-parse", "-q", "--verify", "HEAD"],
        cwd=repo_root,
        env=_git_env(repo_root),
        capture_output=True,
        check=False,
    )
    return proc.returncode == 0


def _git_short_head(repo_root: Path) -> str:
    proc = subprocess.run(
        ["git", "-C", str(repo_root), "rev-parse", "--short", "HEAD"],
        cwd=repo_root,
        env=_git_env(repo_root),
        capture_output=True,
        text=True,
        check=True,
    )
    return proc.stdout.strip()


def _git_show_bytes(repo_root: Path, rev_path: str) -> bytes:
    proc = subprocess.run(
        ["git", "-C", str(repo_root), "show", rev_path],
        cwd=repo_root,
        env=_git_env(repo_root),
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        stderr = proc.stderr.decode("utf-8", errors="replace").strip()
        raise ManPageError(f"`git show {rev_path}` failed: {stderr}")
    return proc.stdout


def _check_header_and_lf(raw: bytes, label: str, name: str, version: str) -> str | None:
    """Verify LF line endings and the `.TH` version header even when mandown is absent."""
    if not raw:
        return f"error: {label} is missing or empty — run 'just man' and commit the result."
    if b"\r" in raw:
        return f"error: {label} contains carriage returns (CRLF) — run 'just man' to normalise to LF."
    first_line = raw.partition(b"\n")[0]
    expected_th = expected_th_header(name, version)
    if first_line != expected_th:
        return (
            f"error: {label} is stale (line 1 is {first_line.decode('utf-8', errors='replace')!r}, "
            f"expected {expected_th.decode('utf-8')!r}) — run 'just man' and commit the result."
        )
    return None


def check_pages(
    repo_root: Path = REPO_ROOT,
    renderer: Renderer | None = None,
    mandown_lookup: Callable[[str], str | None] = shutil.which,
) -> tuple[int, str]:
    """Verify worktree and HEAD man pages are current. Returns (exit_code, message)."""
    if renderer is None and mandown_lookup("mandown") is not None:
        renderer = render_with_mandown

    # 1. Worktree check (.TH header + LF always; full mandown render when available).
    version = parse_cargo_version((repo_root / "Cargo.toml").read_text(encoding="utf-8"))
    for md_rel, name, man_rel in PAGES:
        man_path = repo_root / man_rel
        actual = man_path.read_bytes() if man_path.is_file() else b""
        hdr_err = _check_header_and_lf(actual, man_rel.as_posix(), name, version)
        if hdr_err is not None and renderer is None:
            return 1, hdr_err
        if renderer is not None:
            expected = renderer(repo_root / md_rel, name, version)
            if actual != expected:
                preview = _unified_diff_preview(
                    actual, expected, str(man_rel.as_posix()), f"{man_rel.as_posix()} (rendered)"
                )
                msg = (
                    f"error: {man_rel.as_posix()} is stale — run 'just man' and commit the result.\n"
                    "       (the .TH line embeds the version, so a version bump always changes it)"
                )
                if preview:
                    msg = f"{msg}\n{preview}"
                return 1, msg

    # 2. HEAD check, against itself.
    if not _git_has_head(repo_root):
        return 0, (
            f"man pages are current (version {version}); "
            "no HEAD commit, committed-tree check skipped"
        )

    head_cargo = _git_show_bytes(repo_root, "HEAD:Cargo.toml").decode("utf-8")
    head_version = parse_cargo_version(head_cargo)

    if renderer is None:
        for _, name, man_rel in PAGES:
            committed_man = _git_show_bytes(repo_root, f"HEAD:{man_rel.as_posix()}")
            hdr_err = _check_header_and_lf(
                committed_man, f" the COMMITTED {man_rel.as_posix()}", name, head_version
            )
            if hdr_err is not None:
                return 1, hdr_err
        return 0, (
            f"man pages .TH headers are current (worktree {version}, HEAD {head_version}); "
            "mandown not installed — skipping body re-render (cargo install mandown)"
        )

    # Temp files live inside `man/` rather than `/tmp`: on a Syncthing-synced tree a
    # cross-filesystem `mv` from tmpfs carries the `user_tmp_t` SELinux label into `~/Sync`
    # and wedges the folder (~/AGENTS.md §12). Nothing is moved here, but keeping temp files
    # inside `man/` preserves the invariant if anyone later adds a rename.
    man_dir = repo_root / "man"
    with tempfile.TemporaryDirectory(prefix=".man-check.", dir=man_dir) as tmp:
        tmp_dir = Path(tmp)
        for md_rel, name, man_rel in PAGES:
            committed_md = _git_show_bytes(repo_root, f"HEAD:{md_rel.as_posix()}")
            committed_man = _git_show_bytes(repo_root, f"HEAD:{man_rel.as_posix()}")
            tmp_md = tmp_dir / md_rel.name
            tmp_md.write_bytes(committed_md)
            expected_head = renderer(tmp_md, name, head_version)
            if committed_man != expected_head:
                short_sha = _git_short_head(repo_root)
                preview = _unified_diff_preview(
                    committed_man,
                    expected_head,
                    f"HEAD:{man_rel.as_posix()}",
                    f"HEAD:{man_rel.as_posix()} (rendered for {head_version})",
                )
                msg = (
                    f"error: the COMMITTED {man_rel.as_posix()} (HEAD {short_sha}) is stale for the\n"
                    f"       committed Cargo.toml version {head_version}, although the worktree copy is current.\n"
                    "       Usually a commit or --amend made without staging man/. Fix:\n"
                    "         git add man/etr.1 man/etrs.1 && git commit --amend --no-edit"
                )
                if preview:
                    msg = f"{msg}\n{preview}"
                return 1, msg

    return 0, f"man pages are current (worktree {version}, HEAD {head_version})"


# --------------------------------------------------------------------------------------
# Self-test
# --------------------------------------------------------------------------------------

def _stub_renderer(md_path: Path, name: str, version: str) -> bytes:
    """Deterministic roff renderer so self-tests exercise every guard with or without mandown."""
    body = md_path.read_bytes().replace(b"\r\n", b"\n")
    raw = f'.TH "{name}" 1\n.SH "NAME"\n'.encode("utf-8") + body
    return rewrite_th_header(raw, name, version)


def _git_fixture(root: Path, *args: str) -> subprocess.CompletedProcess[bytes]:
    """Run a `git` command against the throwaway `root` fixture in a fully isolated env."""
    return subprocess.run(
        ["git", "-C", str(root), *args],
        cwd=root,
        env=_git_env(root, isolate_config=True),
        check=True,
        capture_output=True,
    )


def _init_fixture_repo(root: Path, version: str = "0.1.0") -> None:
    (root / "man").mkdir(parents=True, exist_ok=True)
    (root / "Cargo.toml").write_bytes(
        f'[package]\nname = "etr"\nversion = "{version}"\n'.encode("utf-8")
    )
    (root / "man" / "etr.1.md").write_bytes(b"# etr(1)\nClient manual.\n")
    (root / "man" / "etrs.1.md").write_bytes(b"# etrs(1)\nServer manual.\n")
    code, _ = build_pages(root, renderer=_stub_renderer)
    assert code == 0
    for args in (
        ("init", "-q"),
        ("config", "user.email", "test@example.com"),
        ("config", "user.name", "Test"),
        ("config", "core.autocrlf", "false"),
        ("config", "commit.gpgsign", "false"),
        ("config", "tag.gpgsign", "false"),
        ("add", "Cargo.toml", "man/etr.1.md", "man/etrs.1.md", "man/etr.1", "man/etrs.1"),
        ("commit", "-q", "--no-gpg-sign", "--no-verify", "-m", "initial"),
    ):
        _git_fixture(root, *args)


def self_test() -> int:
    failures: list[str] = []

    def assert_true(label: str, cond: bool, detail: str = "") -> None:
        if not cond:
            failures.append(f"{label}: {detail}")

    # 1. Header rewrite normalises CRLF and replaces only line 1.
    rewritten = rewrite_th_header(b'.TH "ETR" 1\r\n.SH "NAME"\r\netr\r\n', "ETR", "0.9.9")
    assert_true(
        "rewrite_th_header normalises CRLF and sets .TH",
        rewritten == b'.TH "ETR" "1" "" "etr 0.9.9" "User Commands"\n.SH "NAME"\netr\n',
        f"got {rewritten!r}",
    )

    # 2. Missing mandown: --check still validates .TH version and LF, --build refuses (exit 1).
    with tempfile.TemporaryDirectory() as d:
        root = Path(d)
        _init_fixture_repo(root)
        c_code, c_msg = check_pages(root, renderer=None, mandown_lookup=lambda _: None)
        assert_true("missing mandown checks .TH and skips body render", c_code == 0 and "skipping" in c_msg, c_msg)
        (root / "Cargo.toml").write_bytes(b'[package]\nname = "etr"\nversion = "0.9.9"\n')
        c_stale_code, c_stale_msg = check_pages(root, renderer=None, mandown_lookup=lambda _: None)
        assert_true("missing mandown still catches stale .TH version", c_stale_code == 1 and "man/etr.1 is stale" in c_stale_msg, c_stale_msg)
        b_code, b_msg = build_pages(root, renderer=None, mandown_lookup=lambda _: None)
        assert_true("missing mandown fails build", b_code == 1 and "mandown is required" in b_msg, b_msg)

    # 3. Clean baseline -> worktree mutations -> HEAD-vs-itself (#81) -> WIP worktree ahead of HEAD.
    with tempfile.TemporaryDirectory() as d:
        root = Path(d)
        _init_fixture_repo(root, "0.1.0")

        code, msg = check_pages(root, renderer=_stub_renderer)
        assert_true(
            "clean fixture baseline passes",
            code == 0 and "worktree 0.1.0, HEAD 0.1.0" in msg,
            f"code={code} msg={msg!r}",
        )

        # 3a. Worktree version bump without `just man` must fail.
        (root / "Cargo.toml").write_bytes(b'[package]\nname = "etr"\nversion = "0.1.1"\n')
        code, msg = check_pages(root, renderer=_stub_renderer)
        assert_true(
            "worktree version bump without rebuild fails",
            code == 1 and "man/etr.1 is stale" in msg,
            f"code={code} msg={msg!r}",
        )

        # 3b. Rebuilding in the worktree makes worktree=0.1.1 while HEAD=0.1.0; both are
        #     internally consistent, so `check_pages` must pass during uncommitted work.
        b_code, _ = build_pages(root, renderer=_stub_renderer)
        assert_true("build_pages succeeds for 0.1.1", b_code == 0)
        code, msg = check_pages(root, renderer=_stub_renderer)
        assert_true(
            "uncommitted worktree ahead of HEAD passes when both trees are self-consistent",
            code == 0 and "worktree 0.1.1, HEAD 0.1.0" in msg,
            f"code={code} msg={msg!r}",
        )

        # 3c. Committing Cargo.toml at 0.1.1 WITHOUT staging man/ (#81 regression) must fail
        #     the HEAD check even though the worktree copies on disk are already at 0.1.1.
        _git_fixture(root, "add", "Cargo.toml")
        _git_fixture(root, "commit", "-q", "--no-gpg-sign", "--no-verify", "-m", "bump without man")
        code, msg = check_pages(root, renderer=_stub_renderer)
        assert_true(
            "HEAD internal inconsistency (#81) caught while worktree is current",
            code == 1 and "the COMMITTED man/etr.1" in msg and "0.1.1" in msg,
            f"code={code} msg={msg!r}",
        )

        # 3d. Amending the man pages into HEAD restores consistency.
        _git_fixture(root, "add", "man/etr.1", "man/etrs.1")
        _git_fixture(
            root, "commit", "-q", "--amend", "--no-edit", "--no-gpg-sign", "--no-verify"
        )
        code, msg = check_pages(root, renderer=_stub_renderer)
        assert_true(
            "amending man pages into HEAD restores clean state",
            code == 0 and "worktree 0.1.1, HEAD 0.1.1" in msg,
            f"code={code} msg={msg!r}",
        )

        # 3e. CRLF bytes planted in man/etrs.1 on disk must fail the byte-exact worktree check.
        lf_bytes = (root / "man" / "etrs.1").read_bytes()
        (root / "man" / "etrs.1").write_bytes(lf_bytes.replace(b"\n", b"\r\n"))
        code, msg = check_pages(root, renderer=_stub_renderer)
        assert_true(
            "CRLF byte drift in man/etrs.1 caught",
            code == 1 and "man/etrs.1 is stale" in msg,
            f"code={code} msg={msg!r}",
        )

    # 4. Git environment & config isolation (#89 review): inherited GIT_DIR / GIT_WORK_TREE /
    #    GIT_INDEX_FILE (as set by git hooks or worktrees) and global commit.gpgsign / hooksPath
    #    must neither corrupt an outer repository nor fail the fixture.
    with tempfile.TemporaryDirectory() as d:
        base = Path(d)
        victim = base / "victim"
        victim.mkdir()
        _init_fixture_repo(victim, "9.9.9")
        _git_fixture(victim, "config", "user.name", "RealUser")
        _git_fixture(victim, "config", "user.email", "real@example.com")
        victim_head_before = _git_short_head(victim)

        poison_gitconfig = base / "poison.gitconfig"
        poison_gitconfig.write_text(
            "[commit]\n\tgpgsign = true\n"
            "[gpg]\n\tprogram = /nonexistent/gpg-must-not-run\n"
            "[core]\n\thooksPath = /nonexistent/hooks-must-not-run\n",
            encoding="utf-8",
        )

        fixture = base / "fixture"
        fixture.mkdir()
        saved_env = os.environ.copy()
        try:
            os.environ["GIT_DIR"] = str(victim / ".git")
            os.environ["GIT_WORK_TREE"] = str(victim)
            os.environ["GIT_INDEX_FILE"] = str(victim / ".git" / "index")
            os.environ["GIT_CONFIG_GLOBAL"] = str(poison_gitconfig)
            os.environ["GIT_CONFIG_COUNT"] = "2"
            os.environ["GIT_CONFIG_KEY_0"] = "commit.gpgsign"
            os.environ["GIT_CONFIG_VALUE_0"] = "true"
            os.environ["GIT_CONFIG_KEY_1"] = "gpg.program"
            os.environ["GIT_CONFIG_VALUE_1"] = "/nonexistent/gpg-must-not-run"
            os.environ["GIT_CONFIG_PARAMETERS"] = (
                "'commit.gpgsign=true' 'gpg.program=/nonexistent/gpg-must-not-run'"
            )

            _init_fixture_repo(fixture, "0.2.0")
            code, msg = check_pages(fixture, renderer=_stub_renderer)
        finally:
            os.environ.clear()
            os.environ.update(saved_env)

        assert_true(
            "fixture succeeds under poisoned GIT_DIR and commit.gpgsign",
            code == 0 and "worktree 0.2.0, HEAD 0.2.0" in msg,
            f"code={code} msg={msg!r}",
        )
        victim_name = _git_fixture(victim, "config", "user.name").stdout.decode("utf-8").strip()
        victim_email = _git_fixture(victim, "config", "user.email").stdout.decode("utf-8").strip()
        victim_head_after = _git_short_head(victim)
        assert_true(
            "outer repository config and HEAD untouched by fixture under GIT_DIR",
            victim_name == "RealUser"
            and victim_email == "real@example.com"
            and victim_head_after == victim_head_before,
            f"name={victim_name!r} email={victim_email!r} head={victim_head_before}->{victim_head_after}",
        )

    if failures:
        for f in failures:
            print(f"man_pages.py self-test FAILED — {f}", file=sys.stderr)
        return 1
    print(f"man_pages.py self-test passed (template v{TEMPLATE_VERSION})")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--build", action="store_true", help="render man/etr.1 and man/etrs.1")
    group.add_argument("--check", action="store_true", help="verify worktree and HEAD man pages")
    group.add_argument("--self-test", action="store_true", help="run internal self-tests")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()
    try:
        if args.build:
            code, msg = build_pages()
        else:
            code, msg = check_pages()
    except ManPageError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    stream = sys.stdout if code == 0 else sys.stderr
    print(msg, file=stream)
    return code


if __name__ == "__main__":
    sys.exit(main())
