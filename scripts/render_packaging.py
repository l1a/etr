#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 l1a
"""Render packaging/{aur,copr,homebrew} for a released version.

WHY THIS EXISTS
---------------
The AUR PKGBUILD and .SRCINFO, the COPR spec and the Homebrew formula all need the version
of the release they package, and three of the four need checksums. A checksum cannot be
computed before the tag exists, so the tempting answer is to *record* the released version
in the repository after each tag -- which means writing one fact down in five places and
keeping them in step forever.

This repo has not paid that bill yet only because it had two channels. Its sibling `retch`
has: `packaging/aur/PKGBUILD` there reached **eleven releases** of drift (0.6.12 in-repo
while the AUR served 0.6.23) with every CI run green, and its post-tag packaging commit left
`main` naming a version that was never released -- which came within one command of
uploading an untagged `retch-cli 0.17.4` to crates.io. Adding COPR and Homebrew here takes
etr from two recordings to five, i.e. straight into that failure mode.

So the templates record NOTHING, and the version is supplied once, at publish time, from the
tag. A stale checksum cannot be committed because no checksum is committed, and the guards
in scripts/packaging_check.py assert only that the templates still carry their sentinels --
a much smaller claim, because the failure they would otherwise hunt is now unrepresentable.

WHAT IT DOES NOT DO
-------------------
No network. The caller computes each sha256 from the artifact it actually downloaded
(`just publish-aur` / `just brew-publish`) and passes it in; this only substitutes and
validates. Staying offline is what lets `--self-test` run inside `just check` on every
machine, including the Windows and macOS hosts where it must work without Git's `usr/bin`
on PATH.

THE RULE THAT MATTERS MOST
--------------------------
**A substitution that matches nothing is a hard error, and a sentinel that survives
rendering is a hard error.** Both directions have burned this family of repos: a sibling's
nix-hash helper silently emitted the PREVIOUS release's hash for a whole release because its
substitution had stopped matching, and a `@VERSION@` left in a published PKGBUILD is a
package nobody can build.
"""

from __future__ import annotations

import argparse
import re
import sys
import time
from dataclasses import dataclass
from pathlib import Path

TEMPLATE_VERSION = 1

REPO_ROOT = Path(__file__).resolve().parent.parent

# Any @UPPERCASE@ token is a sentinel. Deliberately broad: the post-render check asserts that
# NONE remain, so a sentinel someone adds to a template without teaching this script about it
# fails loudly instead of shipping verbatim.
SENTINEL_RE = re.compile(r"@[A-Z0-9_]+@")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
VERSION_RE = re.compile(r"^[0-9]+(\.[0-9]+)+$")

# The %changelog entry the COPR spec gets at render time. A constant rather than
# `git config user.name`: this runs inside mock, where there is no git configuration and no
# git repository at all, and an entry attributed to whoever happened to trigger the build is
# worse than one attributed to the project.
MAINTAINER = "Ken Tobias <634380+l1a@users.noreply.github.com>"

# etr ships TWO binaries on TWO architectures, so its AUR package pins four binary checksums
# where every sibling pins one -- plus a fifth for the arch-independent `etr-extras.tar.gz`
# (both man pages and bash/zsh/fish completions), which a `-bin` package has no other way to
# obtain. They are named for the asset they belong to; `just publish-aur` downloads each asset
# and passes its digest under the matching key.
AUR_SHA_KEYS = (
    "SHA_ETR_X86_64",
    "SHA_ETRS_X86_64",
    "SHA_ETR_AARCH64",
    "SHA_ETRS_AARCH64",
    "SHA_EXTRAS",
)


class RenderError(Exception):
    """The render could not be completed safely."""


@dataclass(frozen=True)
class Target:
    path: str
    sha_keys: tuple[str, ...]
    what: str


# The AUR is two files rendered from one set of values, which is precisely why they are
# rendered together: PKGBUILD and .SRCINFO restate the same version and the same five
# checksums, and hand-maintaining that agreement is the classic AUR footgun. `.SRCINFO` is
# never hand-written.
TARGETS = {
    "aur-pkgbuild": Target("packaging/aur/PKGBUILD.in", AUR_SHA_KEYS, "AUR PKGBUILD"),
    "aur-srcinfo": Target("packaging/aur/SRCINFO.in", AUR_SHA_KEYS, "AUR .SRCINFO"),
    "copr": Target("packaging/copr/etr.spec", (), "COPR spec"),
    "brew": Target("packaging/homebrew/etr.rb", ("SHA256",), "Homebrew formula"),
}


def cargo_version(text: str) -> str:
    """Return the `[package]` version from a Cargo.toml.

    Section-aware on purpose. A bare `grep '^version'` happens to work on this manifest
    today -- it is what the justfile has always used -- but a `[workspace.package]` table or
    a `[dependencies.x]` carrying its own `version` would make it answer about the wrong
    thing, and answer confidently. That is the failure mode this family of repos keeps
    recording, so the parser that feeds the SRPM build does not have it.
    """
    section = None
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            section = stripped[1:-1]
            continue
        if section != "package":
            continue
        m = re.match(r'^version\s*=\s*"([^"]+)"', stripped)
        if m:
            return m.group(1)
    raise RenderError("no [package] version found in Cargo.toml")


def repo_version(root: Path = REPO_ROOT) -> str:
    return cargo_version((root / "Cargo.toml").read_text(encoding="utf-8"))


def validate_version(version: str) -> str:
    if not VERSION_RE.match(version):
        raise RenderError(f"not a version number: {version!r}")
    return version


def validate_sha256(sha: str, key: str) -> str:
    if not SHA256_RE.match(sha):
        raise RenderError(
            f"{key}: not a 64-character lowercase hex sha256: {sha!r} "
            "(compute it from the real artifact; SKIP and placeholders are refused)"
        )
    # A digest of one repeated character is arithmetically possible and has never once been
    # real. Refusing it costs nothing and catches the obvious hand-written placeholder.
    if len(set(sha)) == 1:
        raise RenderError(f"{key}: sha256 is a single repeated character -- placeholder? {sha!r}")
    return sha


def substitute(text: str, values: dict[str, str], *, what: str) -> str:
    """Replace each @KEY@ with its value, requiring at least one hit for every key."""
    for key, value in values.items():
        token = f"@{key}@"
        # The replacement is escaped because re.sub treats a backslash in the replacement as
        # an escape. No value here ever contains one today; this costs nothing and removes a
        # whole class of surprise if one ever does.
        text, n = re.subn(re.escape(token), value.replace("\\", "\\\\"), text)
        if n == 0:
            raise RenderError(
                f"{what}: {token} matched nothing -- the template no longer carries it. "
                "Rendering a file that merely LOOKS updated is the defect this refuses."
            )
    leftover = SENTINEL_RE.findall(text)
    if leftover:
        raise RenderError(
            f"{what}: sentinel(s) survived rendering: {sorted(set(leftover))}. "
            "Refusing to write a file that would be published with a placeholder in it."
        )
    return text


def prepend_changelog(spec: str, version: str, stamp: str) -> str:
    """Insert a %changelog entry for `version` directly under the `%changelog` header.

    rpm's changelog is newest-first, and rpmlint reports an entry whose version disagrees
    with `Version:` (`incoherent-version-in-changelog`). Generating the entry in the same
    pass that renders `Version:` is what makes disagreement impossible.

    IDEMPOTENT, and not as hypothetical tidiness: re-rendering a version the changelog
    already documents would give rpm two entries for one version-release, which rpmlint also
    reports. Re-running a failed publish is exactly when that would happen.
    """
    if re.search(rf"^\*.*-\s*{re.escape(version)}-\S*\s*$", spec, re.M):
        return spec
    entry = f"* {stamp} {MAINTAINER} - {version}-1\n" f"- Update to {version}\n" "\n"
    out, n = re.subn(r"^%changelog\n", f"%changelog\n{entry}", spec, count=1, flags=re.M)
    if n != 1:
        raise RenderError("COPR spec has no %changelog section to prepend to")
    return out


def render(
    target: str,
    version: str,
    shas: dict[str, str] | None = None,
    *,
    root: Path = REPO_ROOT,
    stamp: str | None = None,
) -> str:
    """Return the rendered text for `target` at `version`."""
    if target not in TARGETS:
        raise RenderError(f"unknown target {target!r}; expected one of {sorted(TARGETS)}")
    spec = TARGETS[target]
    validate_version(version)
    shas = shas or {}

    values = {"VERSION": version}
    missing = [k for k in spec.sha_keys if k not in shas]
    if missing:
        raise RenderError(f"{spec.what} pins checksum(s) {missing}: they must be supplied")
    extra = [k for k in shas if k not in spec.sha_keys]
    if extra:
        raise RenderError(f"{spec.what} pins no checksum named {extra}")
    for key in spec.sha_keys:
        values[key] = validate_sha256(shas[key], key)

    text = (root / spec.path).read_text(encoding="utf-8")
    if target == "copr":
        # Before substitution, so the generated entry cannot re-introduce a sentinel and so
        # the leftover check below sees the finished file.
        text = prepend_changelog(
            text, version, stamp or time.strftime("%a %b %d %Y", time.gmtime())
        )
    return substitute(text, values, what=spec.what)


# --------------------------------------------------------------------------------------
# Self-test
# --------------------------------------------------------------------------------------

_SHA = "77ccf85843d24ac3216ab31d2584ff4a95869266c59ddb8bc83819425cfc2033"
_SHA2 = "1e5c3f0a9b8d7c6e5f4a3b2c1d0e9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e"
_SHA3 = "3c1f7e9a2b8d4c6e0f5a9b3d7c1e5f9a2b6d8c4e0f7a1b5d9c3e7f1a5b9d3c7e"
_AUR_SHAS = {
    "SHA_ETR_X86_64": _SHA,
    "SHA_ETRS_X86_64": _SHA2,
    "SHA_ETR_AARCH64": _SHA2,
    "SHA_ETRS_AARCH64": _SHA,
    # A third distinct value, so the `aur pair agrees` loop below cannot pass by accident on a
    # template that put the wrong (but equal) checksum against the extras tarball.
    "SHA_EXTRAS": _SHA3,
}


def _self_test() -> int:
    failures: list[str] = []

    def check(name: str, cond: bool, detail: str = "") -> None:
        if not cond:
            failures.append(f"{name}: {detail}")

    def refuses(name: str, fn) -> None:
        try:
            fn()
        except RenderError:
            return
        failures.append(f"{name}: expected a RenderError, got none")

    # ---- the real templates render, and carry exactly what was asked for ----
    pkgbuild = render("aur-pkgbuild", "1.2.3", _AUR_SHAS)
    check("aur pkgver", "\npkgver=1.2.3\n" in pkgbuild, "rendered PKGBUILD has no pkgver=1.2.3")
    check("aur sha x86", _SHA in pkgbuild, "rendered PKGBUILD is missing a checksum")
    check("aur no sentinel", not SENTINEL_RE.search(pkgbuild), "sentinel survived")

    srcinfo = render("aur-srcinfo", "1.2.3", _AUR_SHAS)
    check("srcinfo pkgver", "pkgver = 1.2.3" in srcinfo, "rendered .SRCINFO has no pkgver")
    check("srcinfo no sentinel", not SENTINEL_RE.search(srcinfo), "sentinel survived")

    # The AUR pair restate one fact twice, so the ONE property worth asserting is that both
    # rendered files agree about every checksum. This is the eleven-release drift, caught at
    # render time rather than by a guard comparing two recordings after the fact.
    for key, value in _AUR_SHAS.items():
        check(
            f"aur pair agrees on {key}",
            (value in pkgbuild) and (value in srcinfo),
            "a checksum reached only one of PKGBUILD/.SRCINFO",
        )

    brew = render("brew", "1.2.3", {"SHA256": _SHA})
    check("brew url", "/refs/tags/v1.2.3.tar.gz" in brew, "rendered formula has no versioned url")
    check("brew sha", f'sha256 "{_SHA}"' in brew, "rendered formula has no sha256")
    check("brew no sentinel", not SENTINEL_RE.search(brew), "sentinel survived")

    copr = render("copr", "1.2.3", stamp="Mon Jan 05 2026")
    check("copr version", "\nVersion:        1.2.3\n" in copr, "rendered spec has no Version: 1.2.3")
    check("copr no sentinel", not SENTINEL_RE.search(copr), "sentinel survived")
    newest = re.search(r"^%changelog\n\* [^\n]*- ([0-9.]+)-(\S+)$", copr, re.M)
    check(
        "copr changelog coherent",
        newest is not None and newest.group(1) == "1.2.3",
        f"newest changelog entry is {newest.group(1) if newest else None!r}, not 1.2.3",
    )

    # Rendering a version the history already documents must not duplicate its entry.
    already = re.search(r"^%changelog\n\* [^\n]*- ([0-9.]+)-", copr, re.M)
    if already:
        v = already.group(1)
        again = render("copr", v, stamp="Tue Feb 03 2026")
        n = len(re.findall(rf"^\*.*- {re.escape(v)}-", again, re.M))
        check("copr render is idempotent", n == 1, f"rendering {v} again produced {n} entries")

    # ---- every refusal, because a renderer that cannot refuse is not a guard ----
    refuses("bad version", lambda: render("aur-pkgbuild", "not-a-version", _AUR_SHAS))
    refuses("bad sha", lambda: render("brew", "1.2.3", {"SHA256": "SKIP"}))
    refuses("short sha", lambda: render("brew", "1.2.3", {"SHA256": _SHA[:63]}))
    refuses("uppercase sha", lambda: render("brew", "1.2.3", {"SHA256": _SHA.upper()}))
    refuses("placeholder sha", lambda: render("brew", "1.2.3", {"SHA256": "0" * 64}))
    refuses("sha for copr", lambda: render("copr", "1.2.3", {"SHA256": _SHA}))
    refuses("missing sha", lambda: render("brew", "1.2.3"))
    # A partially-supplied AUR set must fail rather than render three of four checksums.
    refuses(
        "partial aur shas",
        lambda: render("aur-pkgbuild", "1.2.3", {"SHA_ETR_X86_64": _SHA}),
    )
    refuses("unknown target", lambda: render("nope", "1.2.3"))

    # A template that has LOST its sentinel must fail rather than render unchanged.
    refuses("sentinel gone", lambda: substitute("pkgver=0.1.2\n", {"VERSION": "1.2.3"}, what="t"))
    # A template carrying a sentinel this script does not know about must also fail.
    refuses(
        "unknown sentinel",
        lambda: substitute("a=@VERSION@ b=@WHAT@\n", {"VERSION": "1.2.3"}, what="t"),
    )

    # ---- Cargo.toml parsing ----
    manifest = (
        "[workspace]\nmembers = [\".\"]\n\n"
        '[package]\nname = "etr"\nversion = "9.9.9"\n\n'
        '[dependencies]\nclap = "4.6"\n\n'
        '[dependencies.quinn]\nversion = "0.11.11"\n'
    )
    check("cargo version", cargo_version(manifest) == "9.9.9", f"got {cargo_version(manifest)!r}")
    refuses("cargo no package", lambda: cargo_version('[dependencies]\nversion = "1.0.0"\n'))
    # The live manifest must parse, since the COPR SRPM build reads it.
    check(
        "live cargo version",
        VERSION_RE.match(repo_version()) is not None,
        f"repo_version() returned {repo_version()!r}",
    )

    if failures:
        for f in failures:
            print(f"  FAIL {f}", file=sys.stderr)
        print(f"render_packaging.py self-test FAILED ({len(failures)})", file=sys.stderr)
        return 1
    print(f"render_packaging.py self-test passed (template v{TEMPLATE_VERSION})")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--self-test", action="store_true", help="run built-in tests and exit")
    ap.add_argument(
        "--print-version",
        action="store_true",
        help="print Cargo.toml's [package] version and exit",
    )
    ap.add_argument("--target", choices=sorted(TARGETS), help="which packaging file to render")
    ap.add_argument("--version", help="released version (default: Cargo.toml's)")
    ap.add_argument(
        "--sha256",
        action="append",
        default=[],
        metavar="KEY=HEX",
        help="a checksum, repeatable. For brew use SHA256=<hex>; for the AUR targets supply "
        "all four of " + ", ".join(AUR_SHA_KEYS),
    )
    ap.add_argument("--out", help="write here instead of stdout")
    args = ap.parse_args()

    if args.self_test:
        return _self_test()

    try:
        if args.print_version:
            print(repo_version())
            return 0
        if not args.target:
            ap.error("--target is required (or use --print-version / --self-test)")
        shas: dict[str, str] = {}
        for item in args.sha256:
            key, sep, value = item.partition("=")
            if not sep:
                raise RenderError(f"--sha256 wants KEY=HEX, got {item!r}")
            shas[key.strip()] = value.strip()
        text = render(args.target, args.version or repo_version(), shas)
    except RenderError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    if args.out:
        out = Path(args.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        # Written with an explicit newline policy: these files are consumed by makepkg,
        # rpmbuild and Homebrew on hosts where git may hand out CRLF, and a CR in a PKGBUILD
        # breaks it for Arch users.
        out.write_text(text, encoding="utf-8", newline="\n")
        print(f"rendered {TARGETS[args.target].what} -> {out}")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
