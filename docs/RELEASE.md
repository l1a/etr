# Release, Packaging, Wiki & CI Reference (`etr`)

Loaded on demand before tagging, publishing, updating `PROTOCOL.md`/`README.md`/the GitHub wiki, or editing packaging templates and CI workflows.
*(See `~/.agents/docs/ops/rust-ci-packaging.md` for shared cross-repo CI, AUR, COPR, Homebrew, Scoop, and Claude Code Review invariants.)*

---

## 1. Pre-PR Documentation & Parity Checklists
- **Man pages (`man/etr.1.md`, `man/etrs.1.md` -> committed `man/etr.1`, `man/etrs.1`):** Run `just man` (requires `mandown`) after every edit or version bump and commit the rendered pages (`man-check` verifies both worktree and `HEAD` against `Cargo.toml` because tag tarballs carry only tracked files for COPR/Homebrew).
- **Config, `PROTOCOL.md`, `README.md`:** If `config.toml` keys change, update `~/.config/etr/config.toml` comments, `NOTES.md`, and the `Configuration` wiki page. If wire protocol changes (stream tags, protobufs, handshake), update `PROTOCOL.md`.
- **GitHub Wiki (`https://github.com/l1a/etr.wiki.git`):** Update before opening the PR (`Home.md`, `Getting-Started.md`, `How-It-Works.md`, `Configuration.md`, `Cryptography.md`, `Troubleshooting.md`, `Development.md`, `Compared-to-et-and-mosh.md`).
- **Offline checks (`just check`):** `fmt-check`, `clippy`, `standard-check`, `man-check`, `packaging-check`, `text-check`, `wip-check`.

---

## 2. Packaging Architecture (Six Channels)
- **Single source of truth:** `packaging/metadata.toml` governs crates.io (`etr`), AUR (`etr-terminal-bin`), COPR (`kentobias/etr`), Homebrew (`l1a/homebrew-etr`), Scoop (`l1a/scoop-etr`), and GitHub About (`just github-metadata`). (`brew audit` `<= 80` chars, rpmlint `Summary:` `<= 79`.)
- **Templates:** `@VERSION@`/`@SHA...@` sentinels rendered by `scripts/render_packaging.py` — never paste concrete versions/checksums (even inside `#` comments!).
- **Adding binaries, man pages, or completions:** Update **all four** of COPR spec `%install`/`%files` (`etr.1*` glob because rpm compresses man pages), Homebrew formula `install` (`std_cargo_args` already passes `--locked`), PKGBUILD `package()`, and the `extras` job in `.github/workflows/release.yml` (AUR `-bin` has no source tree and gets man pages/completions from `etr-extras.tar.gz`). Never execute a downloaded binary in `package()` (cross-arch breakage; CI asserts `x86_64` and `aarch64` completions match byte-for-byte).
- **Release job artifact filter:** `pattern: release-*` ensures raw per-arch completion uploads do not publish as loose release assets.

---

## 3. Releasing & Publishing Sequence
```bash
git checkout main && git pull --ff-only && git status --porcelain   # must be clean
git tag vX.Y.Z && git push origin vX.Y.Z                            # starts release.yml + copr.yml
gh run watch "$(gh run list --workflow=release.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
just publish                                                        # crates.io -> AUR -> Homebrew -> Scoop
                                                                    # refuses unless HEAD == tag v<VERSION>
```
- **Prerequisites:** AUR SSH key for `etr-terminal-bin`, push access to `l1a/homebrew-etr` & `l1a/scoop-etr`, COPR `kentobias/etr` with **"Enable internet access during builds" ON** + non-empty `COPR_{LOGIN,USERNAME,TOKEN}` secrets (`copr.yml` skips on forks, fails on `l1a/etr`). COPR API token expires **2027-02-27** (renew CLI config and GitHub secrets together).
- **Verify all 6 channels after publish:**
  - `gh release view vX.Y.Z`
  - `curl -sS -A etr-release-check https://crates.io/api/v1/crates/etr | grep -o '"max_version":"[^"]*"'`
  - AUR: verify with fresh `git clone ssh://aur@aur.archlinux.org/etr-terminal-bin.git /tmp/aurcheck` (RPC `v5` and `cgit` `.SRCINFO` both cache stale data for minutes)
  - `copr-cli get-package --name etr kentobias/etr`
  - `git ls-remote https://github.com/l1a/homebrew-etr HEAD` & `l1a/scoop-etr HEAD`
- **Never run `just install`/`just install-tag`** for local updates — update via `cargo install-update`.
- **Never run `git clean -fdx` without `--exclude=target --exclude=WIP.md --exclude=.claude`** (preview with `-ndx` first).

---

## 4. `Claude Code Review` Workflow Notes
Hardened `claude-code-review.yml` + `.claude/commands/ci-code-review.md` (`v0.11.3`, ported from `retch`). Keep `CLAUDE_CODE_OAUTH_TOKEN` synced in Actions & Dependabot secrets; inspect `anthropics/claude-code-action` compare diffs before merging Dependabot bumps (`install-mcp-server.ts`, `parse-tools.ts`, `parse-sdk-options.ts`, `run-claude-sdk.ts`, `restore-config.ts`, `run.ts`). See `~/.agents/docs/ops/rust-ci-packaging.md` §1.
