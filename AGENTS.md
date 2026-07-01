# AI Agent Guidelines (AGENTS.md)

Welcome! This file contains project-specific guidelines, constraints, and instructions for
all AI assistants (Gemini, Claude, etc.) contributing to the **etr** project.

This file has two parts:

- **Part 1 — Portable Core**: rules that are identical across all of Ken's repos using this
  pattern (currently `etr` and `retch`). If you change wording here, propagate the same
  change to the Portable Core section in sibling repos so they stay in sync.
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
| **Man page regen (4.5)** | Verifies mandown still builds both man pages; the version header must match the bumped version. |
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
4. Regenerates man pages (`just man`) and fails if `mandown` errors out. `man/build/` is gitignored, so there is nothing to diff or commit here — this step only proves the man pages still build and the version header is current.
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
- [ ] `man/build/` is gitignored — do not commit its contents.

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
- [ ] Known gaps section updated: mark completed items as done (strikethrough), add
      new gaps discovered during the work.
- [ ] "Current state" header version and description updated to match the new version.
- [ ] Test count in the test-coverage table updated if tests were added or removed.

### 4.10 Version bump & release hygiene
- [ ] Bump the version in `Cargo.toml` following semver:
      patch (`0.x.N+1`) for bug fixes, minor (`0.x+1.0`) for new features.
- [ ] `Cargo.lock` updated (`cargo build` or `cargo check` does this automatically).
- [ ] `just man` re-run after the bump so the man page version header is current.
- [ ] Before tagging, verify `git status` is completely clean (no modified tracked
      files, no staged changes). The tag must only be created from a clean `main`.
- [ ] Clean up any residual test/profiling artifacts in the working tree before
      tagging: profile captures (`.json.gz`, `*.profdata`), temporary log files,
      any other gitignored scratch files produced during development. Run
      `git clean -ndx --exclude=target` to preview what would be removed, then
      `git clean -fdx --exclude=target` to remove it. This is safe even with
      multiple branches in progress — gitignored files are not part of any
      branch's tracked state, so cleaning them never affects other branches or PRs.
- [ ] `cargo publish` must **never** use `--allow-dirty`. If publish requires that
      flag, stop: something tracked was left uncommitted. Commit or discard it first.

### 4.11 Wiki
Update the GitHub wiki **before opening the PR** (not deferred to publish time).
Clone `https://github.com/l1a/etr.wiki.git`, edit the relevant pages, and push:
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

## 5. Merging
After a PR is merged, run `just merge-pr` to switch to `main`, pull, delete the local
feature branch, and reset `WIP.md` (`Active Branch: none (main is current)`, latest
commit updated).

---
