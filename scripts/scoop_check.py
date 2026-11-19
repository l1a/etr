#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 l1a
"""Guard the Scoop manifest template (packaging/scoop/etr.json).

Run by `just scoop-check` (and therefore `just check` and `just pr`) and by the `scoop` job in
`.github/workflows/packaging.yml`. Needs no network and no Scoop installation.

Like the AUR PKGBUILD, the COPR spec and the Homebrew formula, the manifest is a template:
it carries `@VERSION@`, `@SHA256_X86_64@` and `@SHA256_ARM64@` sentinels that `just scoop-publish`
fills in from the GitHub Release assets when publishing to the bucket repository
(`l1a/scoop-etr`). Nothing is committed back here after a release, so the manifest in this repo
can never drift behind a tag.

Note that Windows only ships the `etr` client (`etrs` is Unix-only by design), and `release.yml`
names the Windows ARM64 binary `etr-windows-aarch64.exe`.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

from render_packaging import SENTINEL_RE, RenderError, render

TEMPLATE_VERSION = 1

REPO = Path(__file__).resolve().parent.parent
MANIFEST_PATH = REPO / "packaging" / "scoop" / "etr.json"
FAKE_SHA_X64 = "a" * 63 + "b"
FAKE_SHA_ARM = "c" * 63 + "d"


def check_manifest_text(text: str) -> list[str]:
    """Return every problem found in `text`; empty list means the template is sound."""
    problems: list[str] = []

    try:
        data = json.loads(text)
    except json.JSONDecodeError as exc:
        return [f"manifest is not valid JSON: {exc}"]

    if data.get("version") != "@VERSION@":
        problems.append(
            "version is not '@VERSION@' (the manifest is a template; do not commit a concrete "
            "released version)"
        )

    if data.get("homepage") != "https://github.com/l1a/etr":
        problems.append("homepage must be 'https://github.com/l1a/etr'")

    if data.get("bin") != "etr.exe":
        problems.append("bin must be 'etr.exe'")

    arch = data.get("architecture")
    if not isinstance(arch, dict):
        problems.append("missing 'architecture' object")
    else:
        x64 = arch.get("64bit")
        if not isinstance(x64, dict):
            problems.append("missing architecture.64bit entry")
        else:
            expected_x64_url = (
                "https://github.com/l1a/etr/releases/download/v@VERSION@/"
                "etr-windows-x86_64.exe#/etr.exe"
            )
            if x64.get("url") != expected_x64_url:
                problems.append(f"architecture.64bit.url must be {expected_x64_url!r}")
            if x64.get("hash") != "@SHA256_X86_64@":
                problems.append("architecture.64bit.hash must be '@SHA256_X86_64@'")

        arm = arch.get("arm64")
        if not isinstance(arm, dict):
            problems.append("missing architecture.arm64 entry")
        else:
            expected_arm_url = (
                "https://github.com/l1a/etr/releases/download/v@VERSION@/"
                "etr-windows-aarch64.exe#/etr.exe"
            )
            if arm.get("url") != expected_arm_url:
                problems.append(f"architecture.arm64.url must be {expected_arm_url!r}")
            if arm.get("hash") != "@SHA256_ARM64@":
                problems.append("architecture.arm64.hash must be '@SHA256_ARM64@'")

    if data.get("checkver") != "github":
        problems.append("checkver must be 'github'")

    autoupdate = data.get("autoupdate")
    if not isinstance(autoupdate, dict) or not isinstance(autoupdate.get("architecture"), dict):
        problems.append("missing autoupdate.architecture object")
    else:
        au_arch = autoupdate["architecture"]
        au_x64 = au_arch.get("64bit", {})
        au_arm = au_arch.get("arm64", {})
        if (
            au_x64.get("url")
            != "https://github.com/l1a/etr/releases/download/v$version/etr-windows-x86_64.exe#/etr.exe"
        ):
            problems.append(
                "autoupdate.architecture.64bit.url must rename etr-windows-x86_64.exe to #/etr.exe"
            )
        if (
            au_arm.get("url")
            != "https://github.com/l1a/etr/releases/download/v$version/etr-windows-aarch64.exe#/etr.exe"
        ):
            problems.append(
                "autoupdate.architecture.arm64.url must rename etr-windows-aarch64.exe to #/etr.exe"
            )

    if re.search(r'"hash"\s*:\s*"[0-9a-fA-F]{64}"', text):
        problems.append("a concrete 64-hex hash is committed; hashes belong only in the bucket")

    return problems


def check_live_manifest() -> list[str]:
    text = MANIFEST_PATH.read_text(encoding="utf-8")
    problems = check_manifest_text(text)
    if problems:
        return problems

    try:
        rendered = render(
            "scoop",
            "1.2.3",
            {"SHA256_X86_64": FAKE_SHA_X64, "SHA256_ARM64": FAKE_SHA_ARM},
        )
    except RenderError as exc:
        return [f"render('scoop') failed on live manifest: {exc}"]

    if SENTINEL_RE.search(rendered):
        problems.append("a @SENTINEL@ survived rendering")
    try:
        rdata = json.loads(rendered)
    except json.JSONDecodeError as exc:
        return [f"rendered manifest is not valid JSON: {exc}"]

    if rdata.get("version") != "1.2.3":
        problems.append("rendered manifest did not carry version 1.2.3")
    if rdata["architecture"]["64bit"]["hash"] != FAKE_SHA_X64:
        problems.append("rendered manifest did not carry the x86_64 hash")
    if rdata["architecture"]["arm64"]["hash"] != FAKE_SHA_ARM:
        problems.append("rendered manifest did not carry the arm64 hash")
    return problems


def _self_test() -> int:
    failures: list[str] = []

    def expect_ok(label: str, probs: list[str]) -> None:
        if probs:
            failures.append(f"{label}: expected clean, got {probs}")

    def expect_bad(label: str, probs: list[str], needle: str) -> None:
        if not any(needle in p for p in probs):
            failures.append(f"{label}: expected a problem containing {needle!r}, got {probs}")

    good = MANIFEST_PATH.read_text(encoding="utf-8")
    expect_ok("live manifest", check_live_manifest())

    expect_bad(
        "invalid json",
        check_manifest_text("{not valid json"),
        "not valid JSON",
    )
    expect_bad(
        "hardcoded version",
        check_manifest_text(good.replace('"@VERSION@"', '"0.10.24"', 1)),
        "not '@VERSION@'",
    )
    expect_bad(
        "hardcoded x86_64 hash",
        check_manifest_text(good.replace("@SHA256_X86_64@", FAKE_SHA_X64)),
        "must be '@SHA256_X86_64@'",
    )
    expect_bad(
        "hardcoded arm64 hash",
        check_manifest_text(good.replace("@SHA256_ARM64@", FAKE_SHA_ARM)),
        "must be '@SHA256_ARM64@'",
    )
    expect_bad(
        "missing #/etr.exe rename",
        check_manifest_text(good.replace("#/etr.exe", "")),
        "#/etr.exe",
    )
    expect_bad(
        "wrong bin shim",
        check_manifest_text(good.replace('"bin": "etr.exe"', '"bin": "etr-windows-x86_64.exe"')),
        "bin must be 'etr.exe'",
    )

    if failures:
        for f in failures:
            print(f"scoop_check self-test FAIL: {f}", file=sys.stderr)
        return 1
    print(f"scoop_check.py self-test passed (template v{TEMPLATE_VERSION})")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--self-test", action="store_true", help="run built-in self-tests and exit")
    args = ap.parse_args()

    if args.self_test:
        return _self_test()

    problems = check_live_manifest()
    if problems:
        for p in problems:
            print(f"packaging/scoop/etr.json: {p}", file=sys.stderr)
        return 1
    print("[scoop-check] etr.json is a template (records no version, no checksum)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
