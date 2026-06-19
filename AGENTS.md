# AI Agent Guidelines (AGENTS.md)

Welcome! This file contains project-specific guidelines, constraints, and instructions for all AI assistants (Gemini, Claude, etc.) contributing to the **etr** project.

---

## 0. Start of session — required reading

**Before doing any work**, read `NOTES.md` in this directory. It is the authoritative
record of current project state: architecture decisions, known gaps, working features,
and design intent. Do not rely on git history or code inspection alone — NOTES.md
captures context that is not in the code.

---

## 1. Project Overview
`etr` is a Rust implementation of the C++ tool **Eternal Terminal (et)**. It is a remote
shell that automatically reconnects without interrupting the session. See NOTES.md for
full architecture and current status.

---

## 2. Core Developer Guidelines
* **Safety First:** Avoid `unsafe` Rust unless absolutely necessary for low-level system integrations (like PTY allocation). If `unsafe` is used, it must be thoroughly documented with safety comments.
* **Idiomatic Rust:** Follow standard Rust styling, formatting (`rustfmt`), and linting (`clippy`). Prefer standard library constructs and robust, well-established crates (e.g., `tokio` for async, `clap` for CLI parsing).
* **Architecture:** The design should support a client-server architecture similar to the original Eternal Terminal.
* **Testing & Documentation Mandates:**
  * When writing code, always write the corresponding tests to go with it.
  * Always document the code clearly as you go.
  * All new features must include unit or integration tests where applicable.

---

## 3. Workflow & Source Control
* **Branching:** Always create a new feature/fix branch before making modifications. Use the prefix pattern `{feature,fix,chore,etc.}/<branch-name>`.
* **Commits:**
  * Keep commit subject lines under 50 characters, in the imperative mood.
  * Always append the AI attribution line as a trailer: `Assisted-By: <Model Name>` (e.g., `Assisted-By: Gemini 3.5 Flash`).
* **PRs & Merges:** Never push to `main` directly or run background push/commit operations without explicit user confirmation.
* **NOTES.md — update on every commit or push:** Before committing or pushing, update
  `NOTES.md` to reflect any changes to architecture, known gaps, working features, or
  design decisions made during the session. NOTES.md must stay current — a reader
  picking up the project from NOTES.md alone should have an accurate picture.

---

## 4. Pre-PR Checklist

Before opening a pull request — and before each subsequent push to an open PR — you
MUST verify every item below. Work through the list top-to-bottom; do not submit until
all items are satisfied or explicitly marked N/A with a reason.

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
- [ ] Run `just man` and verify it succeeds (requires `pandoc`).
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
Update the GitHub wiki (clone `https://github.com/l1a/etr.wiki.git`, edit, push):
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

---
