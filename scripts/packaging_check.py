#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 l1a
"""Offline guards for packaging/: templates still template, and every channel agrees.

WHAT THIS IS FOR
----------------
etr publishes to five places -- crates.io, GitHub releases, the AUR, COPR and a Homebrew
tap -- and each one restates the project's one-line summary and its licence in its own
vocabulary. `packaging/metadata.toml` is where those are written; this asserts that every
channel's copy still says what that file says.

It deliberately does NOT check versions or checksums, because those are not written down
anywhere: the templates carry `@VERSION@`/`@SHA...@` sentinels and
`scripts/render_packaging.py` fills them in at publish time. What IS checked is that the
sentinels are still present -- a template that has quietly become a concrete file would
render "successfully" while publishing a frozen version. That is the failure this pair of
scripts exists to make unrepresentable, approached from both ends.

WHY EVERY CHECK IS OFFLINE
--------------------------
`just check` runs this on every machine in the fleet, including Windows and macOS hosts with
no `sh` and nothing from Git's usr/bin on PATH. A guard that needs the network is a guard
that gets skipped, and a skipped guard is the thing being defended against.

`--sync-github` is the one exception and is never run by `check`: it PUSHES the About box
text to the repository, so it is an explicit, separate act (`just github-metadata`).
"""

from __future__ import annotations

import argparse
import datetime
import json
import re
import subprocess
import sys
from pathlib import Path

TEMPLATE_VERSION = 1

REPO_ROOT = Path(__file__).resolve().parent.parent

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - only on Python < 3.11
    print(
        "error: this needs Python 3.11+ for tomllib (found "
        f"{sys.version_info.major}.{sys.version_info.minor})",
        file=sys.stderr,
    )
    raise SystemExit(1)

SENTINEL_RE = re.compile(r"@[A-Z0-9_]+@")

# rpm changelog months are always these three-letter English abbreviations, independent of
# the machine's locale -- which is why they are spelled out rather than parsed with
# strptime("%b"), whose output follows LC_TIME and would misparse on a non-English host.
MONTHS = {
    "Jan": 1, "Feb": 2, "Mar": 3, "Apr": 4, "May": 5, "Jun": 6,
    "Jul": 7, "Aug": 8, "Sep": 9, "Oct": 10, "Nov": 11, "Dec": 12,
}

# Each template, and the sentinels it MUST still contain. Naming them individually rather
# than "at least one sentinel" is the point: the AUR pair restates four checksums, and a
# PKGBUILD that lost one of them would still look templated.
REQUIRED_SENTINELS = {
    "packaging/aur/PKGBUILD.in": [
        "@VERSION@",
        "@SHA_ETR_X86_64@",
        "@SHA_ETRS_X86_64@",
        "@SHA_ETR_AARCH64@",
        "@SHA_ETRS_AARCH64@",
    ],
    "packaging/aur/SRCINFO.in": [
        "@VERSION@",
        "@SHA_ETR_X86_64@",
        "@SHA_ETRS_X86_64@",
        "@SHA_ETR_AARCH64@",
        "@SHA_ETRS_AARCH64@",
    ],
    "packaging/copr/etr.spec": ["@VERSION@"],
    "packaging/homebrew/etr.rb": ["@VERSION@", "@SHA256@"],
}


class CheckError(Exception):
    """A packaging guard failed."""


def load_metadata(root: Path = REPO_ROOT) -> dict:
    return tomllib.loads((root / "packaging" / "metadata.toml").read_text(encoding="utf-8"))


def _read(root: Path, rel: str) -> str:
    return (root / rel).read_text(encoding="utf-8")


# --------------------------------------------------------------------------------------
# Individual guards. Each returns a list of problems rather than raising, so one run
# reports everything wrong instead of only the first thing.
# --------------------------------------------------------------------------------------


def check_sentinels(root: Path = REPO_ROOT) -> list[str]:
    """Every template still carries the sentinels the renderer substitutes."""
    problems = []
    for rel, required in REQUIRED_SENTINELS.items():
        try:
            text = _read(root, rel)
        except FileNotFoundError:
            problems.append(f"{rel}: missing")
            continue
        for token in required:
            if token not in text:
                problems.append(
                    f"{rel}: lost its {token} sentinel. If a released value was pasted in, "
                    "revert it -- rendering happens at publish time, not in the repo."
                )
    return problems


def check_short_summary(meta: dict) -> list[str]:
    """Rules for the short one-liner, asserted here so a tap push cannot be the first to know.

    These are `brew audit`'s rules and they are not obvious, which is why `short_summary`
    exists separately from `summary`: the long form is 84 characters (over both brew's 80 and
    rpmlint's 79) and reads as prose rather than as a package description.
    """
    problems = []
    desc = meta["short_summary"]
    if len(desc) > 80:
        problems.append(f"short_summary: {len(desc)} chars, brew audit caps it at 80")
    if re.match(r"^(a|an|the)\s", desc, re.I):
        problems.append(f"short_summary: starts with an article, which brew audit rejects: {desc!r}")
    if desc.endswith("."):
        problems.append("short_summary: ends with a full stop, which brew audit rejects")
    if re.search(r"\betr\b", desc, re.I):
        problems.append("short_summary: contains the formula's own name, which brew audit rejects")
    return problems


def check_channels_agree(meta: dict, root: Path = REPO_ROOT) -> list[str]:
    """Every channel's committed copy of the summary and licence matches metadata.toml."""
    problems = []
    summary = meta["summary"]
    license_id = meta["license"]

    # crates.io reads Cargo.toml. `description` may say MORE than the summary, but it must
    # begin with it -- the one-liner crates.io shows is the first thing users read.
    cargo = _read(root, "Cargo.toml")
    m = re.search(r'^description\s*=\s*"([^"]*)"', cargo, re.M)
    if not m:
        problems.append("Cargo.toml: no description field")
    elif not m.group(1).startswith(summary):
        problems.append(
            f"Cargo.toml description does not begin with metadata.toml's summary.\n"
            f"    summary: {summary!r}\n    Cargo.toml: {m.group(1)!r}"
        )
    m = re.search(r'^license\s*=\s*"([^"]*)"', cargo, re.M)
    if not m:
        problems.append("Cargo.toml: no license field")
    elif m.group(1) != license_id:
        problems.append(f"Cargo.toml license is {m.group(1)!r}, metadata.toml says {license_id!r}")

    # AUR PKGBUILD: pkgdesc="..." and license=('...')
    pkgbuild = _read(root, "packaging/aur/PKGBUILD.in")
    m = re.search(r'^pkgdesc="([^"]*)"', pkgbuild, re.M)
    if not m:
        problems.append("PKGBUILD.in: no pkgdesc")
    elif m.group(1) != summary:
        problems.append(f"PKGBUILD.in pkgdesc is {m.group(1)!r}, metadata.toml says {summary!r}")
    m = re.search(r"^license=\('([^']*)'\)", pkgbuild, re.M)
    if not m:
        problems.append("PKGBUILD.in: no license")
    elif m.group(1) != license_id:
        problems.append(f"PKGBUILD.in license is {m.group(1)!r}, metadata.toml says {license_id!r}")

    # .SRCINFO restates both. It is generated from the same render, but it is a SEPARATE
    # template, so nothing except this check stops the two drifting apart in the repo.
    srcinfo = _read(root, "packaging/aur/SRCINFO.in")
    m = re.search(r"^\s*pkgdesc = (.*)$", srcinfo, re.M)
    if not m:
        problems.append("SRCINFO.in: no pkgdesc")
    elif m.group(1).strip() != summary:
        problems.append(
            f"SRCINFO.in pkgdesc is {m.group(1).strip()!r}, metadata.toml says {summary!r}"
        )
    m = re.search(r"^\s*license = (.*)$", srcinfo, re.M)
    if m and m.group(1).strip() != license_id:
        problems.append(
            f"SRCINFO.in license is {m.group(1).strip()!r}, metadata.toml says {license_id!r}"
        )

    # COPR spec: Summary: and License:
    #
    # The SHORT form, not `summary`: rpmlint reports `summary-too-long` above 79 characters
    # and the long one is 84. The length is re-asserted here against the file rather than
    # only against metadata.toml, so a hand-edit to the spec is caught too.
    spec = _read(root, "packaging/copr/etr.spec")
    m = re.search(r"^Summary:\s+(.*)$", spec, re.M)
    if not m:
        problems.append("etr.spec: no Summary:")
    else:
        got = m.group(1).strip()
        if got != meta["short_summary"]:
            problems.append(
                f"etr.spec Summary: is {got!r}, metadata.toml short_summary says "
                f"{meta['short_summary']!r}"
            )
        if len(got) > 79:
            problems.append(
                f"etr.spec Summary: is {len(got)} chars; rpmlint reports summary-too-long "
                "above 79"
            )
    m = re.search(r"^License:\s+(.*)$", spec, re.M)
    if not m:
        problems.append("etr.spec: no License:")
    elif m.group(1).strip() != license_id:
        problems.append(f"etr.spec License: is {m.group(1).strip()!r}, expected {license_id!r}")

    # Homebrew formula: desc and license
    brew = _read(root, "packaging/homebrew/etr.rb")
    m = re.search(r'^\s*desc\s+"([^"]*)"', brew, re.M)
    if not m:
        problems.append("etr.rb: no desc")
    elif m.group(1) != meta["short_summary"]:
        problems.append(
            f"etr.rb desc is {m.group(1)!r}, metadata.toml says {meta['short_summary']!r}"
        )
    m = re.search(r'^\s*license\s+"([^"]*)"', brew, re.M)
    if not m:
        problems.append("etr.rb: no license")
    elif m.group(1) != license_id:
        problems.append(f"etr.rb license is {m.group(1)!r}, metadata.toml says {license_id!r}")

    return problems


def check_locked_not_dropped(root: Path = REPO_ROOT) -> list[str]:
    """Both source-building channels still pin dependency resolution.

    COPR and Homebrew build with network access and no vendored dependencies, so Cargo.lock
    is the ONLY thing tying what gets resolved to what CI tested. Dropping `--locked` would
    not fail anything -- it would silently start resolving fresh dependencies at package
    build time, which is exactly the kind of change that is invisible until it breaks.
    """
    problems = []
    spec = _read(root, "packaging/copr/etr.spec")
    if not re.search(r"^cargo build --release --locked\s*$", spec, re.M):
        problems.append("etr.spec: `cargo build --release --locked` is gone -- never drop --locked")
    if not re.search(r"^cargo test --release --locked\s*$", spec, re.M):
        problems.append("etr.spec: the %check no longer runs `cargo test --release --locked`")

    brew = _read(root, "packaging/homebrew/etr.rb")
    # Asserting `std_cargo_args` rather than the literal flag: that helper is what SUPPLIES
    # --locked, and writing the flag out explicitly alongside it makes cargo refuse the build
    # ("the argument '--locked' cannot be used multiple times").
    if "std_cargo_args" not in brew:
        problems.append(
            "etr.rb: no std_cargo_args -- it is what passes --locked (and --root/--path)"
        )
    if re.search(r'"--locked"', brew):
        problems.append(
            "etr.rb: --locked is passed explicitly AND by std_cargo_args; cargo rejects the "
            "duplicate outright"
        )
    return problems


def check_both_binaries(root: Path = REPO_ROOT) -> list[str]:
    """Every channel installs etr AND etrs.

    etr is the two-binary case, and every packaging template in this family was written for
    a single binary. A channel that ships only the client looks completely healthy -- it
    installs, it runs, `--version` works -- and simply cannot host a session.
    """
    problems = []
    spec = _read(root, "packaging/copr/etr.spec")
    for b in ("etr", "etrs"):
        if f"%{{_bindir}}/{b}" not in spec:
            problems.append(f"etr.spec: %files does not list %{{_bindir}}/{b}")
        if f"man1/{b}.1*" not in spec:
            problems.append(f"etr.spec: %files does not glob man1/{b}.1* (rpm gzips man pages)")

    brew = _read(root, "packaging/homebrew/etr.rb")
    for b in ("etr", "etrs"):
        if f'man1.install "man/{b}.1"' not in brew:
            problems.append(f"etr.rb: does not install man/{b}.1")
        if f'bin/"{b}"' not in brew:
            problems.append(f"etr.rb: never references bin/{b}")

    pkgbuild = _read(root, "packaging/aur/PKGBUILD.in")
    for b in ("etr", "etrs"):
        if f'"${{pkgdir}}/usr/bin/{b}"' not in pkgbuild:
            problems.append(f"PKGBUILD.in: package() does not install /usr/bin/{b}")
    return problems


def check_changelog_dates(root: Path = REPO_ROOT) -> list[str]:
    """Every `%changelog` entry's weekday must match its date.

    rpm writes changelog dates as `* <Day> <Mon> <DD> <YYYY> …` and rpmbuild warns
    `bogus date in %changelog` when the weekday disagrees with the date. It is only a warning,
    so it does not fail a build -- which is exactly why it needs a check here: the v0.9.0 COPR
    build carried `Sat Sep 13 2026` for a Sunday, and the package shipped regardless.

    **The generator was never the problem, and that is the interesting part.**
    `render_packaging.prepend_changelog` derives the weekday with `strftime`, so an entry it
    writes is always right. But it is deliberately idempotent -- on finding an entry for the
    version already present it declines to add a second -- so the hand-written seed entry it
    skipped over kept its wrong weekday. The guard therefore has to read the committed file,
    not the rendered output, because that is where a human can still get it wrong.
    """
    problems = []
    rel = "packaging/copr/etr.spec"
    try:
        text = _read(root, rel)
    except FileNotFoundError:
        return [f"{rel}: missing"]

    # Anchored to the line start and requiring the full `* Day Mon DD YYYY` shape, so prose
    # containing an asterisk (or a `%description` bullet) is never mistaken for an entry.
    entry_re = re.compile(
        r"^\* (?P<dow>Mon|Tue|Wed|Thu|Fri|Sat|Sun) "
        r"(?P<mon>Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) "
        r"(?P<day>\d{2}) (?P<year>\d{4}) ",
        re.M,
    )
    found = 0
    for m in entry_re.finditer(text):
        found += 1
        try:
            actual = datetime.date(
                int(m.group("year")),
                MONTHS[m.group("mon")],
                int(m.group("day")),
            )
        except ValueError:
            problems.append(
                f"{rel}: changelog entry has an impossible date: "
                f"{m.group('mon')} {m.group('day')} {m.group('year')}"
            )
            continue
        expected = actual.strftime("%a")
        if expected != m.group("dow"):
            problems.append(
                f"{rel}: changelog entry '{m.group(0).strip()}' says {m.group('dow')} but "
                f"{actual.isoformat()} is a {expected} -- rpmbuild warns 'bogus date in "
                f"%changelog'. Hand-written entries are the only ones that can be wrong; "
                f"render_packaging.py derives the weekday."
            )
    if found == 0:
        problems.append(f"{rel}: no %changelog entries matched -- has the format changed?")
    return problems


def check_copr_project_text(meta: dict, root: Path = REPO_ROOT) -> list[str]:
    """The COPR project page's Markdown renders as prose, not as code blocks.

    COPR renders a line indented four or more spaces as a code block, so a paragraph that
    happens to be indented silently becomes a grey monospace box on the public project page.
    Fenced blocks are exempt, which is why the state machine below tracks them.
    """
    problems = []
    for key in ("description", "instructions"):
        rel = meta["copr"][key]
        try:
            text = _read(root, rel)
        except FileNotFoundError:
            problems.append(f"metadata.toml [copr] {key} points at {rel}, which does not exist")
            continue
        if not text.strip():
            problems.append(f"{rel}: empty")
        in_fence = False
        for n, line in enumerate(text.splitlines(), 1):
            if line.lstrip().startswith("```"):
                in_fence = not in_fence
                continue
            if in_fence or not line.strip():
                continue
            if line.startswith("    "):
                problems.append(
                    f"{rel}:{n}: indented 4+ spaces outside a fence -- COPR renders this as a "
                    f"code block: {line[:50]!r}"
                )
    return problems


def check_github_metadata(meta: dict) -> list[str]:
    """The About-box text obeys GitHub's limits, checked offline before any push."""
    problems = []
    gh = meta["github"]
    if len(gh["description"]) > 350:
        problems.append(f"[github] description is {len(gh['description'])} chars; GitHub caps 350")
    topics = gh["topics"]
    if len(topics) > 20:
        problems.append(f"[github] {len(topics)} topics; GitHub allows at most 20")
    for t in topics:
        if not re.fullmatch(r"[a-z0-9][a-z0-9-]*", t):
            problems.append(f"[github] topic {t!r} is not lowercase letters/digits/hyphens")
        if len(t) > 50:
            problems.append(f"[github] topic {t!r} is longer than 50 characters")
    return problems


def run_all(root: Path = REPO_ROOT) -> list[str]:
    meta = load_metadata(root)
    return [
        *check_sentinels(root),
        *check_short_summary(meta),
        *check_channels_agree(meta, root),
        *check_locked_not_dropped(root),
        *check_both_binaries(root),
        *check_changelog_dates(root),
        *check_copr_project_text(meta, root),
        *check_github_metadata(meta),
    ]


# --------------------------------------------------------------------------------------
# --sync-github
# --------------------------------------------------------------------------------------


def sync_github(meta: dict, *, dry_run: bool) -> int:
    """Push the About-box description and topics, then read them back.

    Reading back is the point: `gh repo edit` exits 0 on a request the API accepted and
    partially applied, and topics in particular are set as a whole list. Only a read proves
    the repository now says what metadata.toml says.
    """
    gh = meta["github"]
    repo = "l1a/etr"
    current = json.loads(
        subprocess.run(
            ["gh", "repo", "view", repo, "--json", "description,repositoryTopics"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    )
    cur_desc = current.get("description") or ""
    cur_topics = sorted(
        t["name"] for t in (current.get("repositoryTopics") or [])
    )
    want_topics = sorted(gh["topics"])

    print(f"description  now: {cur_desc!r}")
    print(f"             want: {gh['description']!r}")
    print(f"topics       now: {cur_topics}")
    print(f"             want: {want_topics}")
    if cur_desc == gh["description"] and cur_topics == want_topics:
        print("GitHub metadata already matches packaging/metadata.toml")
        return 0
    if dry_run:
        print("--dry-run: nothing was changed")
        return 0

    cmd = ["gh", "repo", "edit", repo, "--description", gh["description"]]
    for t in gh["topics"]:
        cmd += ["--add-topic", t]
    for t in cur_topics:
        if t not in gh["topics"]:
            cmd += ["--remove-topic", t]
    subprocess.run(cmd, check=True)

    after = json.loads(
        subprocess.run(
            ["gh", "repo", "view", repo, "--json", "description,repositoryTopics"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    )
    ok = (after.get("description") or "") == gh["description"] and sorted(
        t["name"] for t in (after.get("repositoryTopics") or [])
    ) == want_topics
    if not ok:
        print("error: GitHub does not match metadata.toml after the edit", file=sys.stderr)
        return 1
    print("GitHub description and topics synced and verified")
    return 0


# --------------------------------------------------------------------------------------
# Self-test
# --------------------------------------------------------------------------------------


def _self_test() -> int:
    import tempfile

    failures: list[str] = []

    def check(name: str, cond: bool, detail: str = "") -> None:
        if not cond:
            failures.append(f"{name}: {detail}")

    meta = load_metadata()

    # Positive: the live tree passes. If this fails the tree is genuinely wrong, and the
    # message says which guard.
    live = run_all()
    check("live tree clean", not live, "; ".join(live))

    # Negative controls. Each mutates a COPY of the real metadata or a fake tree, so the
    # guard is watched FAILING -- a check never seen to fail is not known to work.
    bad = dict(meta)
    bad["short_summary"] = "A reconnecting remote shell"
    check("short_summary article rejected", bool(check_short_summary(bad)), "leading article allowed")
    bad["short_summary"] = "Reconnecting remote shell over QUIC."
    check("short_summary full stop rejected", bool(check_short_summary(bad)), "trailing stop allowed")
    bad["short_summary"] = "etr reconnecting shell"
    check("short_summary own name rejected", bool(check_short_summary(bad)), "formula name allowed")
    bad["short_summary"] = "x" * 81
    check("short_summary length rejected", bool(check_short_summary(bad)), "81 chars allowed")

    bad = dict(meta)
    bad["summary"] = "something else entirely"
    check("summary drift caught", bool(check_channels_agree(bad)), "a drifted summary passed")
    bad = dict(meta)
    bad["license"] = "MIT"
    check("licence drift caught", bool(check_channels_agree(bad)), "a drifted licence passed")

    bad = dict(meta)
    bad["github"] = dict(meta["github"], description="x" * 351)
    check("github description length", bool(check_github_metadata(bad)), "351 chars allowed")
    bad["github"] = dict(meta["github"], topics=["Not-Lower"])
    check("github topic case", bool(check_github_metadata(bad)), "uppercase topic allowed")
    bad["github"] = dict(meta["github"], topics=[f"t{i}" for i in range(21)])
    check("github topic count", bool(check_github_metadata(bad)), "21 topics allowed")

    # A template that lost its sentinel must be reported. Built as a throwaway tree rather
    # than by editing the real one.
    with tempfile.TemporaryDirectory() as td:
        fake = Path(td)
        for rel in REQUIRED_SENTINELS:
            p = fake / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text("pkgver=0.1.2\n", encoding="utf-8")
        problems = check_sentinels(fake)
        check(
            "sentinel loss caught",
            len(problems) >= len(REQUIRED_SENTINELS),
            f"only {len(problems)} problems for {len(REQUIRED_SENTINELS)} gutted templates",
        )
        # And a missing file must be reported rather than crashing the run.
        (fake / "packaging/copr/etr.spec").unlink()
        check("missing template caught", any("missing" in p for p in check_sentinels(fake)))

    # The changelog weekday guard, watched failing on the exact defect it was written for.
    # v0.9.0 really did ship `Sat Sep 13 2026` for a Sunday, and rpmbuild only warned.
    with tempfile.TemporaryDirectory() as td:
        fake = Path(td)
        (fake / "packaging/copr").mkdir(parents=True)
        spec = fake / "packaging/copr/etr.spec"

        spec.write_text(
            "%changelog\n* Sun Sep 13 2026 X <x@y> - 0.9.0-1\n- ok\n", encoding="utf-8"
        )
        check("changelog correct weekday passes", not check_changelog_dates(fake),
              f"a correct entry was flagged: {check_changelog_dates(fake)}")

        spec.write_text(
            "%changelog\n* Sat Sep 13 2026 X <x@y> - 0.9.0-1\n- the v0.9.0 defect\n",
            encoding="utf-8",
        )
        problems = check_changelog_dates(fake)
        check("changelog wrong weekday caught", len(problems) == 1, f"got {problems}")

        # An impossible date must be reported rather than crash the whole run.
        spec.write_text("%changelog\n* Mon Feb 30 2026 X <x@y> - 1.0-1\n- nope\n", encoding="utf-8")
        check("changelog impossible date caught", bool(check_changelog_dates(fake)))

        # Prose containing an asterisk is not a changelog entry and must not be parsed as one.
        spec.write_text(
            "%description\n* a bulleted line\n\n%changelog\n* Sun Sep 13 2026 X <x@y> - 1.0-1\n",
            encoding="utf-8",
        )
        check("changelog ignores prose asterisks", not check_changelog_dates(fake),
              f"prose was parsed as an entry: {check_changelog_dates(fake)}")

        # A file with no entries at all is a format change, not a pass.
        spec.write_text("Name: etr\n", encoding="utf-8")
        check("changelog absence caught", bool(check_changelog_dates(fake)))

    # The COPR indent trap, in both directions.
    with tempfile.TemporaryDirectory() as td:
        fake = Path(td)
        (fake / "packaging/copr").mkdir(parents=True)
        m = {"copr": {"description": "packaging/copr/d.md", "instructions": "packaging/copr/i.md"}}
        (fake / "packaging/copr/d.md").write_text("fine prose\n", encoding="utf-8")
        (fake / "packaging/copr/i.md").write_text("    indented prose\n", encoding="utf-8")
        problems = check_copr_project_text(m, fake)
        check("copr indent caught", len(problems) == 1, f"expected 1 problem, got {problems}")
        # A fenced code block is legitimate and must NOT be reported.
        (fake / "packaging/copr/i.md").write_text(
            "prose\n\n```\n    deeply indented code\n```\n", encoding="utf-8"
        )
        check("copr fence exempt", not check_copr_project_text(m, fake), "fenced code flagged")

    if failures:
        for f in failures:
            print(f"  FAIL {f}", file=sys.stderr)
        print(f"packaging_check.py self-test FAILED ({len(failures)})", file=sys.stderr)
        return 1
    print(f"packaging_check.py self-test passed (template v{TEMPLATE_VERSION})")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--self-test", action="store_true", help="run built-in tests and exit")
    ap.add_argument(
        "--sync-github",
        action="store_true",
        help="push the About-box description and topics from metadata.toml, then verify",
    )
    ap.add_argument("--dry-run", action="store_true", help="with --sync-github: show, change nothing")
    args = ap.parse_args()

    if args.self_test:
        return _self_test()

    try:
        meta = load_metadata()
        if args.sync_github:
            # Never push text that fails the offline guards.
            problems = run_all()
            if problems:
                for p in problems:
                    print(f"  {p}", file=sys.stderr)
                print("refusing to sync GitHub while packaging checks fail", file=sys.stderr)
                return 1
            return sync_github(meta, dry_run=args.dry_run)
        problems = run_all()
    except (CheckError, FileNotFoundError, KeyError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    if problems:
        for p in problems:
            print(f"  {p}", file=sys.stderr)
        print(f"packaging checks FAILED ({len(problems)})", file=sys.stderr)
        return 1
    print("packaging: templates intact, every channel agrees with metadata.toml")
    return 0


if __name__ == "__main__":
    sys.exit(main())
