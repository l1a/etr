# AI Agent Guidelines (AGENTS.md)

Welcome! This file contains project-specific guidelines, constraints, and instructions for
all AI assistants (Gemini, Claude, etc.) contributing to the **etr** project.

This file has two parts:

- **Part 1 — Portable Core**: rules that are identical across all of Ken's repos using this
  pattern (currently `etr`, `retch` and `rusticprofile`). If you change wording here, propagate
  the same change to the Portable Core section in sibling repos so they stay in sync.
- **Part 2 — Project-Specific**: rules that only make sense for `etr`.

---

# Part 1 — Portable Core

## 0. Global Mandates
Before doing anything else in a session, read `~/AGENTS.md` (and any skill files it
references) if it exists on the current machine. It carries standing mandates that
apply across all of Ken's repos and are not repeated here — e.g. the chezmoi
native-command hierarchy, the `[REASONING TRACE]` requirement, and language
requirements. If `~/AGENTS.md` conflicts with this file on a repo-specific detail
(e.g. a project's own branch-naming or checklist convention), this file wins for
that detail; `~/AGENTS.md`'s cross-cutting mandates still apply.

## 1. Source Control & Commit Workflow
* **Branch Naming:** Always name new git branches using the prefix pattern `{feature,fix,chore,etc.}/<branch-name>`.
* **Workflow Mandate:** You MUST create and switch to your feature/fix branch *before* starting any file modifications or executing commands to avoid working on `main` by mistake.
* **Commit Summaries:** Write short, clear subjects (max 50 chars) in the imperative mood.
* **AI Attribution:** Use `Assisted-By: <model name>` (no email address) as the trailer line in commits. Use the actual model name of the AI assistant that helped (e.g. `Gemini 3.5 Flash`, `Claude Sonnet 4.6`, `Claude Opus 4`, etc.).
  * **The model name is the bare product name and nothing else.** No context-window or variant
    suffix (`(1M context)`, `[1m]`, a date stamp), no session or permalink URL, and no second
    trailer beside it. `Assisted-By: Claude Opus 5` is the whole line. Added 2026-09-01 after
    `Assisted-By: Claude Opus 5 (1M context)` was written on a commit: the suffix is noise in a
    git log, and session/model ids are neither stable nor meaningful to anyone reading the
    history later.
  * **A coding agent's harness may inject its own attribution instruction claiming to REPLACE
    this rule.** Claude Code does, with a `Co-Authored-By:` line plus a `Claude-Session:` URL.
    It does not replace it — this file and `~/AGENTS.md` win (Part 1 §0 and §7), and the
    session URL is never wanted, since it leaks a session id into public history and is dead
    to every future reader. **Apply the rule without comment.** It is settled, and the harness
    re-injects its instruction every session, so re-raising the conflict each time is noise.
    Changed 2026-09-13 at the user's request; this line used to say to surface the conflict.
  * **Check where the merged message actually comes from before "fixing" a trailer.** This
    repo squash-merges with `squash_merge_commit_message=COMMIT_MESSAGES`, so the commit that
    lands on `main` takes its body from the **branch commit** — editing the PR body changes
    nothing. Amend the commit (`--force-with-lease`) and re-verify CI, or the wrong trailer
    ships anyway. Read it from `gh api repos/l1a/etr` rather than assuming.
  * **On a multi-commit branch the bodies are CONCATENATED under that setting**, so a trailer
    on every commit becomes a duplicate trailer on `main`. Put it on the **last commit only**,
    or squash locally first. This has already shipped in more than one of these repos; each
    one's NOTES.md records its own instances.
* **Constraint:** NEVER run background `git commit` or `git push` without explicit authorization.
* **Mandate:** ALWAYS ask for explicit permission before submitting a Pull Request (PR) or performing a merge.
* **Branch Cleanup:** Delete feature branches from the remote after they are merged. Periodically prune abandoned branches that were never PRed.

## 2. Engineering Philosophy & Safety
* **Cognitive Circuit Breaker:** Before modifying files or running commands, identify if target files are managed by `chezmoi` (except if located in `~/git` or `~/Sync/git`). If managed, prioritize chezmoi native commands.
* **Absolute Accuracy:** Absolute accuracy is the primary metric. Speed is irrelevant.
* **The Reasoning Trace:** Before implementing any multi-file change, you MUST output a `[REASONING TRACE]` covering Invariants, Subsystem Impact, and Edge-Cases.
* **Empirical Validation:** Test changes locally (compilation, lints, formatting, and unit tests) before proposing a push. See Part 2 §4 for this project's full Pre-PR Checklist and automated gate.

## 3. Cross-Machine Work Handoff (WIP.md)
Any agent starting a session on a repository utilizing cross-machine sync MUST read `WIP.md` before doing anything else.
* **Purpose:** `WIP.md` is a `.gitignored` file synced via Syncthing/Insync to carry context that cannot be inferred from git history alone (what is partially done, machine specs, active branch, next-step checklists, caveats).
* **When to Update:**
  * When switching to a new branch (clear old content, write new context).
  * Before switching machines or ending a session.
  * After pushing commits that change the state of the work.
  * After a PR is merged (set `Active Branch: none (main is current)`).
  * Whenever the next-step checklist changes.
* **What to Include:**
  1. **Machine**: OS, distro, and architecture of the last saved state (e.g. `Linux Fedora 44 x86_64`).
  2. **Active branch name** and PR URL (if open).
  3. **Latest commit hash** and message.
  4. **What was implemented**: Concise description of new/modified files.
  5. **Bugs fixed**: What went wrong and how it was resolved.
  6. **Current CI state**: Passing/failing.
  7. **Open tasks**: Checkbox list of remaining work.
  8. **How to resume**: Exact shell commands to check out, build, and verify.
  9. **Why this work**: Motivating context.
* **What NOT to Include:** Full code diffs, large file contents, detailed architecture docs.

## 4. Continuous Learning Loop
At the conclusion of any task involving a specific skill:
1. Did you encounter a failure, edge case, or nuance not currently documented in the skill?
2. Did the user have to correct your workflow?
3. If YES to either, you MUST automatically update the corresponding `SKILL.md` file with the new learning and synchronize the change before declaring the task complete.

---

# Part 2 — Project-Specific: etr

## 0. Start of session — required reading

**Before doing any work**, read `NOTES.md` in this directory. It is the authoritative
record of current project state: architecture decisions, known gaps, working features,
and design intent. Do not rely on git history or code inspection alone — NOTES.md
captures context that is not in the code.

## 1. Project Overview
`etr` is a Rust implementation of the C++ tool **Eternal Terminal (et)**. It is a remote
shell that automatically reconnects without interrupting the session. See NOTES.md for
full architecture and current status.

## 2. Core Developer Guidelines
* **Safety First:** Avoid `unsafe` Rust unless absolutely necessary for low-level system integrations (like PTY allocation). If `unsafe` is used, it must be thoroughly documented with safety comments.
* **Idiomatic Rust:** Follow standard Rust styling, formatting (`rustfmt`), and linting (`clippy`). Prefer standard library constructs and robust, well-established crates (e.g., `tokio` for async, `clap` for CLI parsing).
* **Architecture:** The design should support a client-server architecture similar to the original Eternal Terminal.
* **Testing & Documentation Mandates:**
  * When writing code, always write the corresponding tests to go with it.
  * Always document the code clearly as you go.
  * All new features must include unit or integration tests where applicable.

## 3. NOTES.md — update on every commit or push
Before committing or pushing, update `NOTES.md` to reflect any changes to architecture,
known gaps, working features, or design decisions made during the session. NOTES.md must
stay current — a reader picking up the project from NOTES.md alone should have an
accurate picture.

## 4. Pre-PR Checklist

Before opening a pull request — and before each subsequent push to an open PR — you
MUST run `just pr`. It automates most of this checklist and hard-fails on the
unconditional items; the rest is a manual checklist it prints for you to confirm. Do not
run `gh pr create` until `just pr` reports the gate passed.

### STOP — read this before treating anything as optional

Two items are **unconditional** — they apply to every PR without exception, including
doc-only, test-only, and chore PRs. There is no "this is just a small change" carve-out,
and `just pr` will hard-fail the gate if either is missed:

| Step | Why unconditional |
|------|------------------|
| **Man page regen (4.5)** | The rendered pages are TRACKED and their `.TH` line carries the version, so a bump that skips `just man` leaves them stale — and they are what COPR and Homebrew package. |
| **Version bump (4.10)** | Every merged PR changes the codebase; the published version must reflect that. Use **patch** for fixes, tests, and doc improvements; **minor** for new user-visible features. |

Rationalising either of these away — "it's only docs", "it's only tests", "no behaviour
changed" — is incorrect. If you find yourself about to skip 4.5 or 4.10, stop and do
them instead.

NOTES.md (4.9) and the wiki (4.11) are also required on every PR. They must be updated
**before** the PR is opened, not deferred. AGENTS.md itself must be included in the
same PR as any change to the checklist — never pushed to `main` as a standalone commit.

### 4.0 Automated gate — `just pr`
Run `just pr` before opening a PR and before each subsequent push. It runs, in order,
and hard-fails on the first problem:
1. Confirms you are on a feature branch, not `main`.
2. Confirms `Cargo.toml`'s version has been bumped past the last git tag.
3. Confirms `NOTES.md` has a `## Current state: v<version>` header matching the bumped version.
4. Regenerates man pages (`just man`) and fails if `mandown` errors out, then fails again (via `just check`'s `man-check`) if the rendered pages differ from `man/etr.1` / `man/etrs.1` — checked against **both** the worktree and HEAD's own committed sources and version (since v0.10.8), so a commit or `--amend` made without staging `man/` fails even while the worktree is correct. **Since v0.9.0 these are tracked**, because a GitHub tag tarball carries only tracked files and both the COPR spec and the Homebrew formula install a man page out of it. Commit them with the version bump.
5. Runs `cargo check` and fails if `Cargo.lock` changed but wasn't committed.
6. Runs `just check` (`cargo fmt --check` + `cargo clippy --all-targets -D warnings`).
7. Runs `cargo test`.
8. Prints the manual checklist below (4.1–4.12 minus what's automated) and requires an explicit confirmation before printing "gate passed".

### 4.1 Code quality gate
- [ ] `just check` passes — `cargo fmt --check` + `cargo clippy --all-targets -D warnings`
- [ ] `just test` passes — all unit and integration tests green

### 4.2 Tests
- [ ] Every new public function or non-trivial private function has at least one unit test.
- [ ] Every bug fix has a regression test that would have caught the original bug.
- [ ] New user-visible behaviour (connection lifecycle changes, protocol changes, new CLI
      flags) has E2E coverage or an explicit note in the PR explaining why it cannot be
      tested automatically.
- [ ] If performance-sensitive code changed, add or update a criterion benchmark in
      `benches/` and note the before/after numbers in the PR description.

### 4.3 Inline code documentation
- [ ] All new `pub` items (functions, structs, enums, traits, modules) have `///` doc
      comments explaining what they do and any non-obvious invariants.
- [ ] Every `unsafe` block has a `// SAFETY:` comment explaining why it is sound.
- [ ] Non-obvious logic inside function bodies has a brief inline comment explaining
      *why*, not *what*.

### 4.4 CLI & --help text
- [ ] Any new or changed CLI flag has an accurate `clap` `doc` / `about` attribute so
      it appears correctly in `--help` output.
- [ ] `etr --help` and `etrs --help` output look correct after the change.
- [ ] Shell completions still generate without errors:
      `etr --completions bash` and `etrs --completions bash`.

### 4.5 Man pages
- [ ] Run `just man` and verify it succeeds (requires `mandown` — `cargo install mandown`).
- [ ] If a new flag or behaviour was added, update the relevant section in
      `man/etr.1.md` or `man/etrs.1.md` before running `just man`.
- [ ] **Commit the rendered `man/etr.1` and `man/etrs.1`.** They are tracked as of v0.9.0.
      The `.TH` line embeds the version, so *every* version bump changes them — `just check`
      runs `man-check` and fails on a stale page — in the worktree **or in HEAD** (since v0.10.8) —
      so this cannot be forgotten silently, including by an amend that forgot `git add man/`.
- [ ] Rationale, so nobody "tidies" it back: a tag tarball contains only tracked files, and
      the COPR spec and Homebrew formula both `install` a man page from that tarball. While
      the pages lived in a gitignored `man/build/`, neither channel could ship one and
      `just install-tag` had to report them "not tracked at that tag".

### 4.6 Config file
- [ ] If a new config key was added to `config.toml` support, document it in the
      `[client]` or `[server]` section of `~/.config/etr/config.toml` comments and in
      the example TOML block in `NOTES.md` and `Configuration` wiki page.

### 4.7 PROTOCOL.md
- [ ] If the wire protocol changed (new stream tags, new protobuf fields, new
      handshake messages), update `PROTOCOL.md` to match.

### 4.8 README.md
- [ ] If a new user-visible feature, install step, or platform support note was added,
      update `README.md` accordingly.

### 4.9 NOTES.md
- [ ] "Current state" header version updated to match the new version (`just pr` enforces this).
- [ ] "Current state" body updated **only if the change leaves something a future reader needs**
      — a changed default, a new user-visible behaviour, an open follow-up. Routine changes need
      nothing here: `git log` is the changelog.
- [ ] Known gaps / next steps updated: **delete** finished items rather than striking them
      through, and add gaps discovered during the work.
- [ ] "Hard-won lessons" updated **if the work leaves behind a rule** — a trap, a gotcha, a check
      that turned out to answer the wrong question. This is the part of NOTES.md that earns its
      keep; a PR that found nothing surprising adds nothing here.
- [ ] Test count in the test-coverage table updated if tests were added or removed.

> **NOTES.md is not a changelog.** It held a full per-release log until v0.10.10 — ~2,060 lines
> reaching back to v0.4.6, which buried everything load-bearing. Do not reintroduce one. If an
> entry would only say what changed, write a good commit message instead. The same applies to
> `WIP.md`: it carries work **in flight**, not a session history — its own header states its
> scope.

### 4.10 Version bump & release hygiene
- [ ] Bump the version in `Cargo.toml` following semver:
      patch (`0.x.N+1`) for bug fixes, minor (`0.x+1.0`) for new features.
- [ ] `Cargo.lock` updated (`cargo build` or `cargo check` does this automatically).
- [ ] `just man` re-run after the bump so the man page version header is current.
- [ ] Before tagging, verify `git status` is completely clean (no modified tracked
      files, no staged changes). The tag must only be created from a clean `main`.
- [ ] Clean up any residual test/profiling artifacts in the working tree before
      tagging: profile captures (`.json.gz`, `*.profdata`), temporary log files,
      any other gitignored scratch files produced during development.

      > **`git clean -fdx` WOULD DELETE `WIP.md`. Never run it without the
      > exclusions below.** `WIP.md` is gitignored *precisely because* it is the
      > Syncthing-synced cross-machine handoff file that Part 1 §3 requires you to
      > maintain — so the unguarded command destroys the file another section of
      > this document mandates, along with every note in it. `.claude/` (local agent
      > settings) goes the same way. This checklist told agents the command was
      > "safe … gitignored files are not part of any branch's tracked state" until
      > 2026-08-15, when a release run's preview showed `Would remove WIP.md` and it
      > was caught only because the preview was actually read.

      **Always preview, and always read the output:**
      ```bash
      git clean -ndx --exclude=target --exclude=WIP.md --exclude=.claude   # preview
      git clean -fdx --exclude=target --exclude=WIP.md --exclude=.claude   # remove
      ```
      Better still for a routine release, remove the specific artifacts you know
      about (`rm -f *.json.gz`) rather than reaching for a
      whole-tree clean at all. Reserve `git clean` for a tree you have inspected.

      The original claim was half right and that is what made it dangerous:
      cleaning gitignored files genuinely does not affect other branches or PRs. It
      does affect *state that lives only in gitignored files*, which on this repo is
      the entire cross-machine handoff.
- [ ] `cargo publish` must **never** use `--allow-dirty`. If publish requires that
      flag, stop: something tracked was left uncommitted. Commit or discard it first.

### 4.11 Wiki
Update the GitHub wiki **before opening the PR** (not deferred to publish time).
Fast-forward an existing clone first (`git pull --ff-only`), or clone freshly
(`git clone https://github.com/l1a/etr.wiki.git`), edit the relevant pages, and push:
- [ ] **Home.md** — if the one-line project description or quick-start changed.
- [ ] **Getting-Started.md** — if prerequisites, install steps, or connection syntax changed.
- [ ] **How-It-Works.md** — if the connection lifecycle, reconnect logic, stream layout,
      security model, or login record behaviour changed.
- [ ] **Configuration.md** — if new CLI flags, config keys, or port-forwarding syntax
      were added or changed.
- [ ] **Cryptography.md** — if the TLS/QUIC or passkey model changed.
- [ ] **Troubleshooting.md** — if the change fixes a known pain point, add or update
      the relevant troubleshooting entry.
- [ ] **Development.md** — if the build steps, test commands, or test count changed.
- [ ] **Compared-to-et-and-mosh.md** — if a capability gap relative to et or mosh
      was closed.

### 4.12 PR description
- [ ] Title is concise (≤ 70 chars), imperative mood.
- [ ] Body summarises *what* changed and *why* (not just a commit list).
- [ ] Test plan lists manual verification steps the reviewer can follow.

### 4.13 Packaging (five channels since v0.9.0)

`just check` runs `packaging-check`, which covers most of this automatically. The items
here are the ones a human has to decide.

- [ ] If the one-line summary, the description or the licence changed, change it in
      **`packaging/metadata.toml`** and nowhere else first, then update each channel's copy
      until `just packaging-check` passes. That file is the single source of truth for
      crates.io, the AUR, COPR, Homebrew and the GitHub About box.
- [ ] **Never paste a version or a checksum into a packaging template.** They carry
      `@VERSION@` / `@SHA…@` sentinels and are rendered at publish time by
      `scripts/render_packaging.py`. A template that has become a concrete file renders
      "successfully" while publishing a frozen version, which is why `packaging-check`
      asserts every sentinel individually.
- [ ] If a new runtime dependency was added, add it to the COPR spec's `Requires:` and the
      PKGBUILD's `depends=()`. Homebrew resolves Rust deps itself and needs nothing.
- [ ] If a binary, man page or completion was added or renamed, update **all four** of the
      spec's `%install`/`%files`, the formula's `install`, the PKGBUILD's `package()`, and the
      `extras` job in `.github/workflows/release.yml`. The fourth is not optional and is easy
      to miss: the AUR package is a `-bin` package with no source tree, so its man pages and
      completions come out of `etr-extras.tar.gz` — **a PKGBUILD cannot install a file the
      tarball does not carry**, and the two files cannot see each other, so a name changed in
      one and not the other builds fine in CI and fails on a user's machine.
      `packaging-check` asserts both binaries reach every channel, and that every man page and
      completion reaches the AUR at its exact destination path, but it cannot know about a
      third binary or a newly-added shell.
- [ ] After the release, verify each channel actually serves the new version rather than
      assuming the push worked — see §6.

## 5. Merging
After a PR is merged, run `just merge-pr` to switch to `main`, pull, delete the local
feature branch, and reset `WIP.md` (`Active Branch: none (main is current)`, latest
commit updated).

## 6. Releasing to all five channels

etr publishes to **GitHub releases, crates.io, the AUR, COPR and a Homebrew tap**. Two of
those happen by themselves on the tag; three are pushed from a workstation.

**The order is not arbitrary** — the AUR and Homebrew both consume artifacts that only exist
once the GitHub release has been built:

```bash
# 0. On main, clean, at the version you intend to release.
git checkout main && git pull && git status --porcelain     # must be empty

# 1. Tag. This alone starts release.yml (GitHub assets) and copr.yml (COPR rebuild).
git tag v0.9.0 && git push origin v0.9.0

# 2. Wait for release.yml to finish. The AUR and Homebrew steps hard-fail without it.
gh run watch "$(gh run list --workflow=release.yml --limit 1 --json databaseId --jq '.[0].databaseId')"

# 3. crates.io, then the AUR, then Homebrew.
just publish
```

`just publish` **refuses unless `HEAD` is the tag for the version in `Cargo.toml`**. That is
not bureaucracy: `cargo publish` uploads whatever the worktree says and a crates.io version
can be yanked but never deleted. Override with `PUBLISH_ANY_REF=1` only for a genuine
exception, never to get past a surprise.

### Verify afterwards — do not infer a channel from the push output

```bash
gh release view v0.9.0                                       # GitHub
curl -sS -A etr-release-check https://crates.io/api/v1/crates/etr | grep -o '"max_version":"[^"]*"'
curl -sS "https://aur.archlinux.org/cgit/aur.git/plain/.SRCINFO?h=etr-terminal-bin" | grep pkgver
copr-cli get-package --name etr kentobias/etr                # or the COPR web UI
git ls-remote https://github.com/l1a/homebrew-etr HEAD       # tap moved
```

**The AUR serves stale reads for a few minutes after a push, and "check cgit instead" is NOT a
sufficient workaround.** This was believed to be an RPC-only problem until the v0.9.0 release,
when one minutes-old push produced all of this *simultaneously*:

| endpoint | reported |
|---|---|
| `rpc/v5/info` | **stale** — the previously known case |
| `cgit/…/plain/.SRCINFO?h=…` | **stale** — and this is the endpoint the old advice named |
| `cgit/…/plain/PKGBUILD?h=…` | fresh |
| `cgit/…/log/?h=…` | fresh |
| `git clone ssh://aur@aur.archlinux.org/…` | fresh — **authoritative** |

So cgit is not uniformly fresh: two of its views were current while a third was not, and the
stale one was the file being checked. **Verify an AUR push with a fresh clone**, never with a
single HTTP endpoint:

```bash
git clone ssh://aur@aur.archlinux.org/etr-terminal-bin.git /tmp/aurcheck
grep -E '^pkgver' /tmp/aurcheck/PKGBUILD
```

Do not read one endpoint and conclude the push failed; the push output naming a commit range
is itself proof the server-side hook parsed `.SRCINFO` and accepted it.

### One-time prerequisites

These exist as of v0.9.0 and are recorded so a fresh machine or a new maintainer knows what
the recipes assume:

- **AUR**: an SSH key registered with an AUR account that co-maintains `etr-terminal-bin`.
- **Homebrew**: push access to `github.com/l1a/homebrew-etr`. The tap is created by pushing
  to it; `just brew-publish` handles an empty tap, including pinning its default branch.
- **COPR**: the `kentobias/etr` project, with **"Enable internet access during builds" ON**
  (the spec resolves crates.io at build time and there is no vendor tarball), plus the
  `COPR_LOGIN` / `COPR_USERNAME` / `COPR_TOKEN` repository secrets for `copr.yml`. Without
  the secrets that workflow *skips* rather than fails, so a fork does not go red — which also
  means a missing secret looks like success. Check the run's log for the skip notice.

---
