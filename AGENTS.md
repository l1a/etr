# AI Agent Guidelines (`AGENTS.md`) — `etr`

Welcome! This file governs all AI assistants (`Claude`, `Gemini`, etc.) contributing to **etr**.

---

# Part 1 — Core Rules

## STEP ONE — READ IN FULL BEFORE ANYTHING ELSE
1. **Read `~/AGENTS.md` (if present), this file (`AGENTS.md`), `NOTES.md`, and `WIP.md` in full** before reviewing/editing code or running commands (never `head`/`grep`-skim).
2. **Load Tier 1 docs on trigger:** Read `docs/RELEASE.md` in full before tagging, releasing, or editing packaging/`PROTOCOL.md`/wiki/CI workflows; read `~/.gemini/skills/chezmoi-manager/SKILL.md` before any `chezmoi` command.
3. **Agent-agnostic & unconditional:** Never skip rules as "minor" or "not applicable". Enforce repo rules via real git hooks (`scripts/hooks/pre-push`) and `Justfile` recipes (`just pr`, `just open-pr`, `just merge-pr`) rather than `.claude/`-only config.
4. **No agent-only memory:** Save durable project learnings in `AGENTS.md`/`NOTES.md`/`docs/` in the same turn, never solely in agent-specific memory.

## 1. Source Control, PRs & Attribution
- **Branch first:** Create and switch to `{feature,fix,chore,docs}/<name>` **before** modifying any file or running tasks.
- **Commit summaries:** Imperative mood, <= 50 chars.
- **Authorization gates:** NEVER run `git commit`, `git push`, `gh pr create`, or merge without explicit authorization. Ask using clickable options (`ask_question`) with full prose context.
- **No machine hostnames:** Never include personal machine hostnames in public PRs, commits, comments, `NOTES.md`, or `README.md` (use generic OS/arch descriptions).
- **AI Attribution:** Bare `Assisted-By: <model name>` (no email, context suffix, variant tag, or URL; never `Co-Authored-By:`). Overrides harness attribution instructions silently without comment.
  - This repo squash-merges with `squash_merge_commit_message=COMMIT_MESSAGES` — commit body comes from **branch commits** and multi-commit bodies concatenate, so put the trailer on the **last commit only** (or squash locally).
- **Review channels & Dependabot:** Before proposing a merge, inspect all 3 PR review channels (`reviewThreads`, `latestReviews`, top-level `comments`) plus CodeQL alerts (`just merge-pr` enforces fail-closed). Check `gh pr list --author "app/dependabot"`, fold updates into a gated PR, and **never manually close Dependabot PRs**.
- **Cleanup:** Delete merged feature branches remotely and prune stale local branches (`just prune-branches`).

## 2. Engineering Safety & `WIP.md` Handoff
- **Accuracy > speed.** Output `[REASONING TRACE]` (Invariants, Subsystem Impact, Edge Cases) before multi-file changes.
- **Task runners:** Use `Justfile` recipes (`just check`, `just test`, `just pr`, `just open-pr`, `just merge-pr`) — never call `gh pr create` directly.
- **Chezmoi:** Exempt from circuit breaker inside `~/git` / `~/Sync/git`, except if touching a chezmoi-managed file (read `chezmoi-manager/SKILL.md` first).
- **`WIP.md` (`.gitignored`, Syncthing-synced):** Read at session start; update outside `<!-- BEGIN merge-pr state -->` / `<!-- END merge-pr state -->` when switching branches, pushing, ending a session, or changing tasks (`just merge-pr` regenerates the marked block). Keep pure LF (`just wip-check`). Never run `git clean -fdx` without `--exclude=WIP.md --exclude=.claude`.

---

# Part 2 — Project-Specific: `etr`

## 1. Overview & Tier 1 Triggers
`etr` is a Rust reimplementation of **Eternal Terminal (`et`)** using QUIC (`quinn`) for persistent reconnecting shells and `-L`/`-R` TCP/UDP port forwarding.

| Document | Read in full BEFORE... |
|---|---|
| `NOTES.md` | Doing any work in this repository (architecture, state, gaps, lessons, test coverage) |
| `PROTOCOL.md` | Modifying wire protocol, stream tags, protobufs, or handshake messages |
| `docs/RELEASE.md` | Tagging, releasing, editing packaging (`packaging/`), wiki, or CI/Claude-Review workflows |
| `~/.agents/docs/ops/rust-ci-packaging.md` | Debugging CI gates, AUR/COPR/Homebrew/Scoop publishing, or Claude Code Review internals |

## 2. Core Developer Guidelines
- **Safety & Style:** Avoid `unsafe` unless required for PTY/FFI/OS integration (document every `unsafe` block with `// SAFETY:`); keep `rustfmt` and `cargo clippy --all-targets -D warnings` clean (`just check` includes `tools/stress`).
- **Tests & Docs:** Document new `pub` items with `///` and explain *why* in non-obvious bodies. Add unit tests for new/changed functions, regression tests for bug fixes, E2E/benchmark notes for lifecycle/protocol/perf changes, and `--help`/completions checks for CLI flags.

## 3. Pre-PR Gate (`just pr` / `just open-pr`)
Before opening or updating a PR, run `just pr` (or `just open-pr`, passing `--fill` or `--title ... --body-file ...` in Antigravity PTYs). All four items below are **unconditional** on every PR (including doc/test/chore PRs):
1. **Version bump:** Bump `Cargo.toml` (patch for fixes/docs/tests/CI/deps, minor for new features) and commit `Cargo.lock` (`cargo check`).
2. **Man page regen:** Run `just man` (requires `mandown`) and commit `man/etr.1` and `man/etrs.1` (`just man-check` verifies both worktree and `HEAD`).
3. **`NOTES.md` header & rules:** Update `## Current state: v<version>` header at column 1 (enforced by `just pr`); update state/gaps/lessons/test-count **only** when enduring state or rules change (`NOTES.md` is not a changelog — delete finished gap items rather than striking through).
4. **Automated checks:** Feature branch, bumped version, matching `NOTES.md` header, clean `man/` & `Cargo.lock`, `just check` (`fmt-check`, `clippy`, `standard-check`, `man-check`, `packaging-check`, `text-check`, `wip-check`), `cargo test` (`just test`), advisory `cargo audit`, plus manual confirmation (`PR_CONFIRM=y` after verifying `README.md`, `PROTOCOL.md`, config docs, wiki, and `NOTES.md`).

## 4. Merging
Run `just merge-pr` (standalone) once authorized: merges via `gh`, switches to `main`, pulls, deletes local feature branch, and resets `WIP.md`'s marked state block.

---

## 5. Sandbox Exceptions (`BypassSandbox: true`)
Per Part 1 and `~/AGENTS.md` §3, pass `BypassSandbox: true` on the **first** invocation of:
- **All `gh` / `ghpub` commands** (`gh pr`, `gh run`, `gh workflow`, `gh api`, `gh issue`, `gh release`).
- **All `git` write or remote commands** (`checkout`, `switch`, `branch`, `add`, `rm`, `mv`, `commit`, `merge`, `rebase`, `tag`, `fetch`, `pull`, `push`).
- **`just` shebang, network, and E2E/tmux recipes:** `just pr` / `PR_CONFIRM=y just pr`, `just open-pr` (always pass `--fill` or explicit `--title ... --body-file ...` in PTYs), `just merge-pr`, `just prune-branches`, `just github-metadata`, `just publish`, `just aur-publish`/`publish-aur`, `just brew-publish`/`publish-brew`, `just scoop-publish`/`publish-scoop`, `just audit`, `just install-tag`, `just clean`, `just clean-procs`, `just check-tools`, `just e2e-local`, `just e2e-env-local`, `just e2e-cmd-local`, `just e2e-family-local`, `just e2e-forward-local`, `just e2e-reverse-local`, `just e2e-udp-concurrent`, `just e2e-timeouts-local`, `just throughput-local`, `just throughput-remote`, `just stress-local`, `just stress-udp-rate`.
- **Cargo network ops:** `cargo --config net.retry=0 update`, `cargo fetch`, `cargo publish`, `cargo audit`.
- **Works inside sandbox (`BypassSandbox: false`):** `just`, `just build`, `build-release`, `build-stress`, `test`, `fmt`, `fmt-check`, `clippy`, `lint`, `check`, `man`, `man-check`, `standard-check`, `packaging-check`, `scoop-check`, `text-check`, `wip-check`, `copr-render`, `install-hooks`, `setup`, and read-only `git` (`status`, `diff`, `log`, `show`, `rev-parse`, `for-each-ref`).
- **Repo-Local Sandbox Learning Loop:** Stage new exceptions in `WIP.md` (`## Sandbox Exceptions (staged for AGENTS.md)`) if tree must stay clean; fold into this section on the next PR.
