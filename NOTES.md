# etr — Project Status Notes

## What this is

`etr` is a Rust reimplementation of [Eternal Terminal](https://eternalterminal.dev/) (`et`).
Eternal Terminal is a persistent remote shell that survives network interruptions — unlike
SSH, the session keeps running on the server and the client reconnects transparently when
the link drops.  This project uses **QUIC** (via the `quinn` crate) for the transport
layer, which provides reliable, ordered, multiplexed streams with congestion control
and TLS 1.3 built-in.

## Current state: v0.7.7 — `just clean` stops killing live sessions

New in v0.7.7 (tooling only; no Rust change, 112 tests unchanged).

- **`just clean` killed every `etrs` the user owned, including live remote sessions.** Its
  first line was `pkill -x etrs`. Someone clearing build output has not asked for their
  sessions to be dropped, and nothing about the name or the output hinted that it might —
  the damage lands on whoever is *developing remotely on the machine being cleaned*, which
  is exactly this project's own use case.
- **And it was not even cleaning properly.** It ran `cargo clean` and nothing else, so
  three gitignored build outputs survived: `man/build/`, `tools/stress/target/` (a separate
  crate the root `cargo clean` cannot see) and `etrs_fwd.json.gz` — the profile capture
  §4.10 already asks to be cleared before tagging. So the recipe skipped artifacts and
  killed processes: precisely backwards. `clean` now removes all four and touches no
  process or tmux session.
- **New `just clean-procs` owns process cleanup, and shows its work first.** An orphaned
  test server and the server hosting a live session are indistinguishable to `pkill`, but
  not to a human reading start times and arguments — so it lists every `etrs` and
  `stress_tool` owned by the invoking user, plus tmux sessions matching `etr_*` (every test
  session name in the justfile shares that prefix), and only then asks. The confirmation
  takes `CLEAN_CONFIRM`, an interactive stdin, or piped input under a bound, mirroring
  `just pr`'s `PR_CONFIRM` — a cleanup step is often scripted, and a bare `read` there
  blocks on a stdin nobody holds.
  - Uses `pgrep -x`, never `pgrep -f`: a full-command-line match would also match the
    recipe's own shell, whose command line contains the pattern. That self-match is
    recorded in `~/AGENTS.md` §10 as having caused three separate incidents.
  - Restricted to the invoking user, so a shared machine's other users are never touched.
  - **Escalates TERM → KILL**, because SIGTERM is not reliably enough — see the gap below.
    A reap that leaves the process running while reporting success is worse than one that
    never ran.
- *Two defects in the recipe itself, both found by running it rather than reading it:*
  `@echo` inside a `#!/usr/bin/env bash` recipe is **not** just's line-suppression prefix —
  it is handed to bash verbatim and fails with `@echo: command not found`, exit 127, *after*
  the cleaning has happened, so `clean` looked broken when only its last line was. And the
  first structural check that `clean` contains no `pkill` passed on the **comment**
  explaining why it no longer does; stripping comments first is the same discipline
  `scripts/gate_conformance.py` already applies for the same reason.

### Found while doing this, NOT fixed here

- **`etrs` may not exit on SIGTERM when idle.** Four orphaned `etrs` processes left over
  from e2e runs all survived ~3 s of `SIGTERM` and only died on `SIGKILL` (reproduced twice,
  4/4 processes). This contradicts the v0.4.7 note above, which says the reconnect-wait loop
  listens for SIGTERM/SIGHUP and exits cleanly after recording a utmp logout. If that
  handler is not firing for a session whose client is long gone, then **stale utmp entries
  can outlive a killed server** — the exact problem the v0.4.7 work set out to fix. Needs a
  controlled reproduction (start a session, drop the client, `kill -TERM` the server, watch
  the log and `last`) before anything is changed; `clean-procs` escalates to SIGKILL so it
  is not blocked on the answer.
- *Adjacent, deliberately out of scope:* the six e2e recipes' cleanup traps still run
  `pkill -x etrs`, so running `just e2e-local` on a machine with a live remote session kills
  it — the same defect this release fixes in `clean`, in six more places. Fixing it properly
  means each recipe tracking the pid it spawned rather than reaping by name, which is a
  larger change than a rename. Their leftovers are also not hypothetical: four orphans were
  present when `clean-procs` was first run, so the traps do not catch everything today.

## Previous: v0.7.6 — `etr` no longer panics without a controlling terminal

New in v0.7.6 (client fix; 112 unit tests unchanged, two new e2e parts).

- **`etr` panicked with `exit 101` when there was no controlling terminal.**
  `enable_raw_mode()` opens `/dev/tty`, which fails with `ENXIO` under cron, a systemd
  unit, a CI job or an agent shell — and the call was `.unwrap()`, so the user got a Rust
  panic and a backtrace hint instead of a diagnostic. Found during the v0.7.5 work, where
  it masked the real bug with an unrelated failure.
- **The two cases are treated differently, because they genuinely differ.**
  - *A remote command* (`etr host 'cmd'`) does not need a local terminal — there is
    nothing to put in raw mode and nothing to interact with, only output to relay. It now
    runs without raw mode, which makes `etr host 'cmd'` usable from a script, a cron job or
    CI. Everything downstream already coped: every `terminal::size()` call site is an
    `if let Ok`, so no resize is sent and the server keeps its default PTY size.
  - *An interactive session* exits 1 with a message naming the missing terminal and
    pointing at the command form. Degrading there would connect to a shell nobody can type
    at, which — since the remote shell never exits on its own — would hang rather than
    return.
- **The output stays byte-clean, which is the subtle half.** `restore_terminal` writes the
  VT reset sequences to stdout, and its own doc comment already required that it only be
  called when raw mode was actually entered. The degraded path reaches several of its call
  sites, so honouring that by hand at each one would have been fragile — a new
  `RAW_EVER_ENABLED` flag now enforces it inside the function. Without it, 70 bytes of
  escapes would land in a redirected stdout, **corrupting exactly the command output v0.7.5
  existed to deliver**. Measured: 13 bytes out for `echo <sentinel>` with no terminal, all
  of them the output; 80 with a terminal, where the 70-byte reset belongs.
- **The Windows stdin-reader gate fires on both branches.** That reader waits on a one-shot
  signal sent after raw mode is enabled (the v0.6.5 first-line-echo fix); leaving it unsent
  on the degraded path would block a no-terminal Windows run forever on a reader that never
  starts, and piped stdin still has to be relayed.
- **Regression coverage, negative-controlled.** `just e2e-cmd-local` gains Part 5 (a remote
  command runs with no terminal, output present **and containing no ESC byte**) and Part 6
  (an interactive session refuses, non-zero, naming the terminal). Run against the pre-fix
  binary, Part 5 fails with "etr panicked (exit 101)" while Parts 1–4 still pass. The tests
  detach with `os.setsid()` from Python rather than the `setsid(1)` binary, which does not
  exist on macOS.
  - Worth keeping: both checks first had to be rewritten as `CMD || RC=$?`. Under the
    recipe's `set -e` a bare call **aborts before the exit code can be read**, so the
    exit-101 branch was unreachable and Part 6 — where a non-zero exit is the *expected*
    result — could never have passed. It failed with no message at all, which is the same
    "a check that cannot report" shape recorded throughout `~/AGENTS.md`.

## Previous: v0.7.5 — a remote command's output survives redirected stdin

New in v0.7.5 (client fix; 112 unit tests unchanged, two new e2e parts).

- **`etr host 'cmd' </dev/null` printed nothing and exited 0.** `run_session`'s
  wait-for-completion `select!` treated `stdin_task` finishing as the session ending, and
  with redirected stdin that EOF arrives immediately — before the command has produced
  anything. Ending there aborted `pty_recv_task` and `stdout_task` too, so the output was
  discarded and the only bytes written were the 70-byte terminal reset. **Measured, not
  inferred:** under a pty, `echo <sentinel> </dev/null` emitted exactly 70 bytes with the
  sentinel absent, against 81 bytes with it present when stdin was left attached — an
  11-byte difference that is precisely the lost output.
- **The obvious one-line fix was wrong, and the failure is worth recording.** Disabling
  that `select!` arm alone changed the symptom to `[etr] Session ended: connection lost`,
  still with no output. Returning from `stdin_task` **drops `pty_send`, which *finishes*
  the client→server half of the PTY QUIC stream** — the server reads that as the session
  ending and tears the connection down. Half-closing means keeping the sender alive, not
  merely not exiting.
- **A PTY cannot be half-closed, so EOF has to be relayed in-band.** With the stream held
  open, `printf x | etr host cat` then **hung** (killed at 45 s) because `cat` never saw
  EOF: there is no out-of-band "stdin is done" on this stream, and unlike ssh-over-a-pipe
  there is no pipe to close. The client now relays **`VEOF` (`0x04`) once** on stdin EOF —
  exactly what a terminal does on Ctrl-D — then parks, holding the stream open until the
  command exits and the server sends its `Disconnect`. Commands that never read stdin
  never dequeue the byte.
- **Interactive sessions keep the old behaviour, deliberately.** The whole change is gated
  on `has_remote_command`. There the remote side is a shell that never exits on its own,
  so holding the session open past stdin EOF would turn `etr host </dev/null` from a prompt
  return into a hang. Verified: it still exits. Console stdin never EOFs, so the arm never
  fires in normal interactive use regardless.
- **Regression coverage, verified to actually fail without the fix.** `just e2e-cmd-local`
  gains Part 3 (output survives stdin EOF) and Part 4 (a stdin-reading command sees EOF and
  exits, with an elapsed-time assertion so a hang fails loudly instead of passing slowly).
  Run against the pre-fix binary, Part 3 fails reporting the same 70 bytes measured above
  while Parts 1–2 still pass — so the test is specific to this bug rather than to any
  breakage. **These parts deliberately do not use tmux**: a tmux pane's stdin is a pty that
  never EOFs, so a tmux-hosted run cannot reach this path at all. They need a controlling
  terminal *and* a redirected fd 0 simultaneously, which means a real `pty.fork` with
  `</dev/null` inside it.
- `PROTOCOL.md` §5.2 now records both conventions — that finishing the client→server half
  means "session over", and that `VEOF` is how stdin EOF reaches the remote reader. No tag
  or field changed.

### Found while doing this, NOT fixed here

- **`etr` panics when there is no controlling terminal.** `enable_raw_mode().unwrap()`
  (`src/bin/etr.rs`) fails with `ENXIO` ("No such device or address") when `/dev/tty`
  cannot be opened — from cron, a systemd unit, a CI job or an agent shell — and the user
  gets a Rust panic and `exit 101` instead of a diagnostic. Confirmed here: it is what made
  the first attempt to reproduce the bug above fail for an unrelated reason. It needs a
  decision rather than a reflex fix (run degraded without raw mode, or exit with a clear
  message), so it belongs in its own PR.

## Previous: v0.7.4 — NOTES.md tells the truth about its own gaps again

New in v0.7.4 (documentation and repo hygiene only; no Rust change, 112 tests unchanged).

- **The *Known gaps / next steps* list was carrying a gap that shipped closed three
  releases ago.** "`just` recipes unusable from native Windows shells" sat there un-struck
  while the v0.7.1 section, forty lines above it, opens by saying it *closes that exact
  gap and names it*. AGENTS.md §4.9 requires completed gaps be struck through, and this is
  the one file Part 2 §0 tells every agent to read first — so the failure mode is specific:
  an agent reading the authoritative list picks up work that is already done, and the
  evidence that it is done is in the same file. Now struck through with a pointer to v0.7.1.
- **Three claims in the v0.7.1 section were true when written and false the moment v0.7.2
  merged.** They are phrased in the present tense — "does not exist", "there is no
  `install-hooks` recipe", "still true" — so they do not read as history, they read as
  current state: `scripts/hooks/`, `.gitattributes`, `just pr`'s bare `read` and the missing
  `just open-pr`. v0.7.2 fixed all four and says so. The blocks are kept rather than deleted
  (they record *why* the work was deferred, which is worth having) but are now explicitly
  marked superseded, so no reader mistakes them for open items.
- **Two stale figures corrected:** the build-and-install section advertised `cargo test
  (110 tests)` against a suite of 112 — disagreeing with this file's own coverage table — and
  the `install-tag` example pinned 0.7.1. The wiki's `Development.md` carried the same 110.
- **`fix/windows-backspace-and-initial-prompt` deleted, local and remote** (was `55434fb`,
  recoverable from the reflog or by sha). It held the *deferred PTY reader* approach — start
  the PTY master reader on the first connection rather than at session start — which the
  v0.6.4 section of this file records as **attempted and reverted**: it had no effect,
  because the fault is in the Windows *input* path, not the output path. Both dependency
  bumps it carried (`anyhow` 1.0.103, `crossbeam-epoch` 0.9.20) are already on `main`. Per
  AGENTS.md Part 1 §1 it was an abandoned branch to prune; leaving it invited someone to
  rediscover a dead end that is already written down.
- *No behaviour changed and no test was added*, which is exactly why the two unconditional
  steps still ran: the man pages rebuild with the bumped version header, and the version is
  bumped. "It's only docs" is the rationalisation §4 names.


New in v0.7.3 (tooling only; no Rust change, 112 tests unchanged).

- **`just merge-pr` had no CI gate at all.** It went from the branch check straight to
  `gh pr merge --squash --delete-branch`, with no inspection of the status rollup. `gh pr merge`
  will happily merge a red PR when the repository has no branch protection, and "wait for the
  checks to settle" is not "wait for them to pass" — so **every merge in this repo has been
  ungated**, safe only because whoever merged happened to look at CI first.
- `rusticprofile` added this gate in its `v0.1.5` after a PR went in with a leg red, and extended it
  in `0.2.1` after an **empty** rollup passed vacuously, reporting green over a commit CI had never
  seen. Neither reached here — the same cross-repo staleness that left the nushell completion path
  wrong for months, this time on the recipe that performs the irreversible act.
- **Three refusals now:** a failing check; an **empty** rollup, because "nothing ran" is not
  "everything passed" and the failure arm cannot distinguish them; and checks still running, rather
  than racing them. The empty state is compared as a **string** rather than through `jq -e length`,
  because `gh --jq` is gh's built-in jq while an external `jq` is not on a default Windows PATH —
  and a gate that silently degrades where its dependency is missing is the thing being fixed.
- **`scripts/gate_conformance.py` (template v3) is vendored and run by `standard-check`**, which
  `just check` depends on, so these guards cannot quietly vanish again. It asserts nine of them
  across `pr`, `open-pr` and `merge-pr`, **with comments stripped first** — a comment explaining a
  guard must not satisfy the check for a recipe that lost it.
- **Structural, not behavioural, and it says so.** It proves a guard is present, not that it works.
  The install helpers are pure functions their self-test can call; these recipes run the suite, push
  branches and merge PRs, so executing them from `check` would be slow and occasionally destructive.
- **Verified by running it, safely:** on a branch with no PR the rollup is empty, so `merge-pr`
  refuses and exits *before* reaching `gh pr merge` — testing the gate without merging anything.


New in v0.7.2 (tooling and repo hygiene only; no Rust change, 112 tests unchanged). Closes the
four items v0.7.1 recorded as found-but-not-fixed.

- **`scripts/hooks/pre-push` is now tracked, and `just install-hooks` installs it.** Until now
  this repo had no `scripts/hooks/` at all: a hook existed in one machine's `.git/hooks/`,
  untracked, so it was unreproducible and **no fresh clone got a pre-push gate**. `AGENTS.md`
  Part 1 §4 leans on real git hooks as *the* agent-agnostic enforcement layer — which only holds
  if the hook is in the repository. The tracked hook skips on `GIT_NO_CHECK=1` and exits 0 when
  `just` is absent, since refusing a push for someone without the toolchain would make the repo
  unusable rather than safer. `scripts/install_hooks.py` **backs up an existing hook rather than
  clobbering it**, asks `git rev-parse --git-path hooks` instead of assuming `.git/hooks` (wrong
  in a worktree or submodule), and is Python for the same reason the install helpers are: no
  `sh`, no `cygpath`.
- **`.gitattributes` added — `* text=auto eol=lf`.** Both sibling repos have carried this for
  months and this tree is Syncthing-shared across three OSes, so without it a Windows checkout
  writes CRLF, Syncthing propagates it to the Unix clones, and git there reports **every** tracked
  file as modified: a phantom whole-tree diff with zero content change. retch's v0.4.3 measured
  13811 insertions / 13811 deletions, all line-ending flips. This was **live, not theoretical** —
  git printed `LF will be replaced by CRLF` while committing v0.7.1. Binary assets
  (`*.png`, `*.ico`) are pinned so nothing normalises them.
- **`just pr` can be answered without a terminal.** It ended in a bare `read`, so a script, CI job
  or agent blocked on a stdin that would never answer or died without saying why — and that reads
  as **the gate refusing the change**, not as a question nobody could hear. It now accepts
  `PR_CONFIRM`, an interactive stdin, or piped input under a ten-second bound, and the failure
  message names `PR_CONFIRM`. **Not a bypass:** all four paths still require an explicit `y`, so
  this widens *who can answer*, not *what counts as an answer*.
- **`just open-pr` exists.** Previously `AGENTS.md` §4.0 asked, in prose, that `gh pr create` not
  be run until `just pr` passed — which binds nobody. Neither `gh` nor `git` has a hook for "a PR
  is about to open", so a justfile recipe is the only thing that can gate it, and being a recipe it
  binds a human, Claude, Gemini or anything else identically. It also **pushes when the branch has
  no upstream** — otherwise `gh pr create` has no remote branch to open from and fails *after* the
  gate printed "Gate passed", which reads as the gate rejecting work it just approved. Deliberately
  only when there is no upstream: pushing unconditionally would silently publish existing commits.
- **Verified rather than assumed:** all four confirm paths exercised (`PR_CONFIRM=y` answers;
  no-terminal-and-no-stdin **aborts naming the variable**; piped `y` still passes; piped `n` still
  refuses), `install-hooks` run and the installed hook diffed byte-identical to the tracked one,
  and `open-pr`'s push exercised on this PR's own branch — the one condition that cannot be
  reproduced after the fact.
- **Claude Code Review no longer runs on every pull request.** `claude-code-review.yml` is now
  `workflow_dispatch` only; the `pull_request` trigger is kept commented immediately below it, so
  restoring it is uncommenting two lines. The reason is not that the reviews were unwelcome: the
  token behind the action has failed before in a way worse than useless — in `rusticprofile` it
  went from 19 consecutive green runs to failing **every** run in ~490 ms on turn 1 at $0.00,
  posting no findings. A rejection before any tokens are billed is a credential or quota problem,
  not a verdict on the code, but left on `pull_request` it becomes a red check on every future PR
  for a reason unrelated to that PR — which trains everyone to merge over failing checks. And this
  family has already paid for the opposite failure, a review job going **green without reviewing
  anything**. A check that cannot be believed in either direction is not a check.
  - **Deliberately NOT copied from retch:** its `v0.6.17` also sets `if: false` on the job *in
    addition* to keeping `workflow_dispatch`, which means a manual dispatch appears to run and
    silently does nothing. That is the "setting that quietly does nothing" shape all three repos
    exist to refuse, so etr keeps the dispatch genuinely runnable
    (`gh workflow run claude-code-review.yml --ref <branch>`). Worth revisiting in retch.
  - `claude.yml` (the `@claude` mention workflow) is **untouched** and shares the same secret, so
    mentions fail the same way if the token is bad — noted in the workflow rather than left to be
    rediscovered.
- *Why all four existed:* they are `rusticprofile`'s `0.0.21`, `0.2.12`, `install-hooks` and
  `.gitattributes`, none of which reached this repo. The same cross-repo staleness that left the
  nushell completion path wrong here for months. **The `pr`/`open-pr` triad is still not covered by
  the shared standard** — `templates/justfile-common.just` records it as out of scope, because
  these recipes legitimately differ per repo; what is now aligned is their *behaviour*, by hand.


New in v0.7.1 (tooling only; no Rust code change, test count unchanged at 112):

- **Closes the "`just` recipes unusable from native Windows shells" known gap** listed
  below. The install-family recipes were `#!/usr/bin/env bash` shebang recipes, so on
  Windows `just` tries to translate the interpreter path with `cygpath` and every one of
  them fails before running a line. That gap asked for exactly this fix: *"plain
  (non-shebang) recipes"*. The work now lives in two vendored Python helpers, which need
  no `sh`, no `cygpath`, no coreutils and nothing from Git's `usrin`.
- **`install` installed the DEBUG binaries, and no completions at all.** It was
  `install: build` plus `cp target/debug/{etr,etrs}`, so `just install` handed you an
  unoptimised build. It is now `install: install-man install-completions` +
  `cargo install --path .`, which installs **both** binaries in release mode and the man
  pages and completions with them. **`install-release` is therefore removed as redundant**
  — that is a deliberate, user-visible change to two documented recipes.
- **nushell completions went where Windows nushell never looks.** `NU_COMP` was
  `$XDG_CONFIG_HOME/nushell/autoload`; on Windows `$nu.user-autoload-dirs` is exactly
  `%APPDATA%
ushellutoload` — one entry — and nushell never reads the XDG path. Introduced
  in v0.4.24, which set that path *and* changed the output "to denote auto-loaded state".
  So the recipe wrote a real file somewhere nothing consults and reported success.
- **The output asserted two things that were false.** It printed
  `zsh auto-loaded ({{ZSH_COMP}} is in zsh's default $fpath)` — zsh reads completion
  functions **only** from directories on `fpath`, and `site-functions` is not on it by
  default on any distribution — and the same claim for nushell. Both are now *checked*: zsh
  via an **interactive** zsh (a non-interactive one sources neither `.zshrc` nor anything it
  includes, so it reports the built-in default and gets the answer confidently wrong the
  other way), and PowerShell is reported `NOT ACTIVE` on Windows rather than implying
  `$PROFILE` will read a file under `~/.config`.
- **A failed shell no longer reports success.** The helper raises instead of logging to
  stderr and continuing, so `install-completions` cannot print "Installed" over work it did
  not do.
- **New `install-tag VERSION`** — installs a released tag with all three artefacts *from
  that tag*: binaries via `cargo install --git --tag`, completions generated by **the
  installed binary** so they cannot disagree with its CLI, man pages read out of the tag.
  Because `man/build/` is gitignored here, the man-page step correctly reports
  "not tracked at that tag" and leaves the pages alone rather than failing — the binaries
  and completions still install.
- **`just standard-check` runs the helpers' `--self-test`, and `just check` depends on it.**
  Not a text diff: three separate repositories cannot diff each other's files, and a diff
  would pass happily on a repo that never adopted the standard. `scripts/install_completions.py`,
  `scripts/install_man.py` and `templates/justfile-common.just` are vendored **byte-identically**
  across `etr`, `retch` and `rusticprofile` (`TEMPLATE_VERSION = 2`).
- **etr is the two-binary case the standard exists for.** Project facts live in a `PROJECT`
  header (`BINS := "etr etrs"`, `MAN_PAGES := …`) above the block, which is why the block can
  be byte-identical to siblings that ship one binary.

### Two gaps found while doing this, deliberately NOT fixed here — ~~open~~ **both fixed in v0.7.2**

> **Superseded.** The two bullets below, and the paragraph after them, are kept as the record
> of *why* this work was deferred to its own PR — but they are written in the present tense
> and every item in them landed in **v0.7.2**: `scripts/hooks/pre-push` is tracked and
> installed by `just install-hooks`, `.gitattributes` exists, `just pr` accepts `PR_CONFIRM`,
> and `just open-pr` exists. Read them as history, not as open gaps.

- **`scripts/hooks/` does not exist, so no fresh clone gets a pre-push gate.** A `pre-push`
  hook is present in this machine's `.git/hooks/` but is untracked, so it is unreproducible
  and there is no `install-hooks` recipe to restore it. Both sibling repos track their hook
  and install it via `just install-hooks`. `AGENTS.md` Part 1 §4 leans on real git hooks as
  *the* agent-agnostic enforcement layer, so this is a real hole — but adding it is a
  workflow change rather than an install-family fix, and belongs in its own PR.
- **There is no `.gitattributes`.** Both siblings carry `* text=auto eol=lf` precisely
  because this tree is Syncthing-shared across Windows, Linux and macOS; retch's v0.4.3
  records a phantom whole-tree diff (13811 insertions / 13811 deletions, all line-ending
  flips) caused by its absence.

Also unchanged and still true *as of v0.7.1* — ~~and both fixed in v0.7.2~~: `just pr`'s
checklist ends in a bare `read`, so it cannot be answered non-interactively — the same gap
both siblings have (rusticprofile fixed it with a `PR_CONFIRM` env override), and
`just open-pr` does not exist here at all, so there is no gated call site for
`gh pr create`.

## Previous: v0.7.0 — AUR publishing (etr-terminal-bin)

New in v0.7.0:

- **AUR package `etr-terminal-bin`** (x86_64 + aarch64): installs the prebuilt
  `etr` and `etrs` binaries from the GitHub release assets. The AUR names
  `etr` and `etr-bin` were already taken by an unrelated ECMP-traceroute tool
  that also installs `/usr/bin/etr`, so the package is named
  `etr-terminal-bin` and declares `conflicts=('etr' 'etr-bin')` — and
  deliberately does **not** declare `provides=('etr')`, since that would
  wrongly satisfy dependencies on the other tool.
- **`just publish-aur`**: renders `packaging/aur/PKGBUILD.in` and
  `packaging/aur/SRCINFO.in` (single source of truth; `.SRCINFO` is never
  hand-written), computing sha256 checksums from the *actual downloaded*
  GitHub release assets for the current `Cargo.toml` version, then clones
  `ssh://aur@aur.archlinux.org/etr-terminal-bin.git`, commits, and pushes.
  Hard-fails with instructions if the release assets don't exist yet
  (publish ordering: tag → release.yml → crates.io → AUR); exits cleanly
  without an empty commit if the AUR repo already matches. Requires an SSH
  key registered with an AUR account that owns/co-maintains the package.
- **`just publish` now ends by invoking `just publish-aur`**, so a normal
  release publishes crates.io and the AUR in one step. If the AUR step fails
  (e.g. release build not finished), crates.io publication is unaffected —
  re-run `just publish-aur` alone.
- `packaging/` added to the crates.io `exclude` list; README gained an
  "Arch Linux (AUR)" install section.
- No Rust code changes; test count unchanged (112).

## Previous: v0.6.5 — Windows input path + terminal restore on exit

New in v0.6.5 (four independent Windows parity fixes):

- **Special characters no longer "eaten".** The client's stdin reader previously
  used `std::io::stdin().read()`, which on Windows goes through Rust std's
  `ReadConsoleW` shim (UTF-16→UTF-8 + internal line cooking). That shim **drops
  bytes that aren't clean UTF-8**, which is what made special characters
  disappear (e.g. zellij keybindings not registering, needing `^g`). The reader
  now reads the console input handle directly with `ReadFile` (new `read_stdin`,
  Windows path). With `ENABLE_VIRTUAL_TERMINAL_INPUT` already on, `ReadFile`
  returns the same VT byte stream a Unix terminal emits — no UTF-8 mangling.
  `enable_vt_console` also now sets the console **input codepage to UTF-8
  (65001)** (saved and restored on exit) so typed multi-byte characters reach
  the remote as UTF-8, matching what the std path produced. No-op on Unix.
- **First line of input now echoes as typed (issue #54).** The single stdin
  reader thread is spawned before the QUIC connect, but raw + VT-input mode is
  only enabled *after* the connect. On Windows a `ReadFile` issued while the
  console is still in cooked/line mode stays line-buffered for that whole read,
  so the first line was held client-side until Enter (the reported "no echo
  until first Enter"; the `ReadFile` change above did **not** fix this on its
  own — it is a timing problem, not a read-mechanism one). The Windows reader
  now waits on a one-shot signal fired immediately after the first
  `enable_raw_mode` + `enable_vt_console`, so its very first read happens in raw
  + VT mode and is per-keystroke. Unix has no such coupling (and never showed the
  bug), so its reader is ungated and starts immediately as before.
- **Local terminal is restored on exit.** A remote full-screen app
  (zellij/vim/less) puts the *local* terminal into alternate-screen,
  mouse-reporting, bracketed-paste, hidden-cursor, application-keypad and
  scroll-region modes via escapes we relay. On a hard drop (remote reboot) or a
  forced `~.` the remote never sends its cleanup, so those modes were left set:
  the mouse wheel spewed escape sequences and the terminal was unusable.
  `disable_raw_mode` only restores console line/echo flags, not these
  emulator modes. The client now emits an explicit VT reset (`restore_terminal`)
  on every final-exit path. It is split in two: a **cursor-safe** part
  (`TERM_RESET_MODES` — disable mouse/paste/app-keys, show cursor, reset SGR)
  emitted on every exit, and a **screen-restoring** part (`TERM_RESET_SCREEN` —
  leave alternate screen, reset scroll region; both move the cursor to home)
  emitted **only** on unclean exits (`~.`, abandoned hard drop, remote command
  whose TUI may still be up). Clean shell exits skip the screen reset so the
  cursor is left untouched. Cross-platform (Unix terminals honour the same
  resets); deliberately avoids a full RIS so scrollback is preserved.
- **Local shell's Enter works again after etr exits.** `enable_vt_console` sets
  `ENABLE_VIRTUAL_TERMINAL_INPUT` on the console, but crossterm's
  `disable_raw_mode` only ORs the line/echo/processed-input bits back — it never
  clears that VT-input flag. So after etr exited, the console was left with
  VT-input still enabled, and the *local* shell echoed typed characters but did
  not accept Enter (the VT-translated Enter wasn't recognised as line
  submission). etr now captures the console's exact original input/output modes
  and input codepage once (`capture_console_originals`, before raw mode is first
  enabled) and restores them verbatim on exit (`restore_console_state`), which
  clears the leftover VT-input flag. Pre-existing since VT-input was introduced
  in v0.6.4. No-op on Unix (crossterm fully restores termios there).
- Test count: 110 → 112 (two regression tests asserting the reset sequences
  cover the critical modes and never move the cursor on the safe path / never
  clear scrollback).

**Live verification (Windows → WSL Fedora 44 etrs, 2026-07-21):** both fixes were
verified end-to-end against a real Unix `etrs`, driving the rebuilt v0.6.5 client
with *synthesized real console key events* (`WriteConsoleInputW` into the
client's own console — the same INPUT_RECORDs a physical keyboard produces):

- *Fix #1 (input not eaten, per-keystroke):* An isolated harness confirmed the
  exact path `read_stdin` uses (console in raw + `ENABLE_VIRTUAL_TERMINAL_INPUT`,
  read via `ReadFile`) delivers every key intact and unbatched: `a`, **Ctrl+G →
  `0x07`**, Up-arrow → `ESC [ A`, a rapid 5-key burst, and `é` → UTF-8 `c3 a9`.
  A full-composition harness then injected keystrokes into a live `etr` session;
  the remote `zsh-syntax-highlighting` re-coloured the command **character by
  character** as it arrived (proof of per-keystroke delivery, not batching) and
  the typed command executed and round-tripped. This is the root cause of the
  "characters eaten / zellij needs `^g`" report.
- *First-line echo (#54):* A/B harness that types the first line and snapshots
  the client's stdout **before** sending Enter. Pre-fix binary: the typed
  command is absent from the snapshot (held client-side until Enter). With the
  reader gate: the command appears in the pre-Enter snapshot, echoed back
  per-keystroke (remote syntax-highlighting recolours char-by-char) — first line
  now echoes as typed.
- *Fix #2 (terminal restore):* On clean shell exit the client emitted exactly the
  70-byte cursor-safe `TERM_RESET_MODES` (no cursor-moving screen reset),
  confirmed in the live output byte stream.
- *Console-mode restore:* A harness recorded the console input mode before
  launching etr and again after etr exited via `~.`. Result: the mode was
  restored byte-identical (`0x01f7` → `0x01f7`) and `ENABLE_VIRTUAL_TERMINAL_INPUT`
  was not left set — the local shell's line input (Enter) works after exit.

Note (adjacent, pre-existing, out of scope): running `etr host 'cmd'` with
redirected/`</dev/null` stdin ends the session on stdin EOF before the command's
output arrives (`run_session` treats `stdin_task` completing as session end);
interactive console stdin never EOFs so this does not affect normal use.

### ~~Known issue — Windows: first line of input not echoed until Enter~~ (fixed in v0.6.5)

Fixed in v0.6.5 by gating the Windows stdin reader until raw + VT-input mode is
enabled (see the "First line of input now echoes as typed" bullet above). The
`ReadFile`-based input path was necessary but **not sufficient** on its own — the
first line was line-buffered because the first read was *issued* before raw mode,
which is a timing problem the reader gate solves. Verified with an A/B harness
that snapshots the client's stdout before Enter is sent. Tracked in
[GitHub issue #54](https://github.com/l1a/etr/issues/54).

## Previous: v0.6.4 — Windows Backspace fix

New in v0.6.4:
- **Windows client Backspace fixed.** The Windows console delivers legacy key
  codes to raw byte reads (Backspace → `0x08`), whereas a Unix PTY expects the
  xterm convention `0x7f` (DEL) to match the default `stty erase`. The client
  now enables virtual-terminal console modes after raw mode
  (`ENABLE_VIRTUAL_TERMINAL_INPUT` on stdin, `ENABLE_VIRTUAL_TERMINAL_PROCESSING`
  on stdout) via a new Windows-only `windows-sys` dependency, so it emits the
  xterm byte sequences the remote expects and renders the remote's ANSI output.
  No-op on Unix. Verified live (Windows `etr` → Unix `etrs`): Backspace now
  erases correctly.
- Bumped `anyhow` 1.0.102→1.0.103 to clear RUSTSEC-2026-0190 (an unsoundness
  advisory against a pre-existing transitive dep). (`crossbeam-epoch` was
  already bumped to 0.9.20 in v0.6.3 for RUSTSEC-2026-0204.)
- Test count: 110 (unchanged).

### Known issue (as of v0.6.4; resolved in v0.6.5) — Windows: first line of input not echoed until Enter

Resolved in v0.6.5 by reading the console input handle directly with `ReadFile`
instead of `std::io::stdin().read()` (see the v0.6.5 section above). The
historical diagnosis is kept below for context.

When connecting from a Windows `etr` client to a Unix host, the shell prompt
renders correctly, but the **first line** the user types is not echoed until
Enter is pressed (after which the whole line appears and runs, and the session
behaves normally thereafter). This does **not** occur linux→linux and is not
shell-specific (reproduced with `zsh`+`zellij`+`starship` and with
`bash --norc`).

Diagnosis: `-vvv` logs show the keystrokes reach the server, but the Windows
client sends the first line as a single batched chunk (with the trailing Enter)
rather than per-keystroke, so the remote PTY echoes the whole line only on
submission. The fault is in the Windows client input path
(`std::io::stdin().read()`), which does not deliver bytes per-keystroke at
session start; a large contributor is the shell's startup terminal-probing,
which — with VT-input mode on — the Windows console auto-answers with a ~7 KB
burst (256-color palette, size, mode) plus mouse-motion events that
back-pressure/batch the reader.

Two fixes were attempted and **reverted** (neither is in this release): (1)
deferring the server-side PTY reader until the first client connection — no
effect, since the input path (not output) is at fault; (2) reading discrete key
events via `crossterm` and translating to bytes on the client — eliminated the
batching but caused the console's terminal-query auto-responses to be echoed
back as visible garbage (`]4;N;rgb:…`). Tracked in
[GitHub issue #54](https://github.com/l1a/etr/issues/54).

## Previous: v0.6.3 — project logo

New in v0.6.3:
- Added `assets/logos/etr-logo.svg` (source) plus rendered `etr-logo-256.png`,
  `etr-logo-512.png`, and a multi-resolution `etr-logo.ico` (256/128/64/48/32/16px).
- `etr.exe` on Windows now carries the logo as its PE icon resource (shown in
  Explorer/taskbar): `windows/etr.rc` references `assets/logos/etr-logo.ico`
  and is compiled/linked by `build.rs` via the `embed-resource` crate
  (Windows-only build-dependency). Uses `manifest_optional()` since the icon
  is cosmetic — a missing resource compiler must not fail the build.
  `etrs.exe` gets the same resource since both binaries share one `build.rs`.
- README.md now displays the logo under the title. Linux/macOS CLI binaries
  have no equivalent "exe icon" resource slot, so no automatic packaging
  change was made there; the README image is the cross-platform display
  point.
- `assets/linux/etr.desktop`: an optional freedesktop `.desktop` launcher
  template (`Exec=etr`, user edits in the host before installing). The
  `linux-x86_64` release job now also copies this plus `etr-icon-256.png`
  into the release dist alongside the binaries, for users who want an
  app-menu entry. Not installed automatically by `just install` — manual
  opt-in only, documented in README's new "Desktop entry (Linux)" section.
- Test count: unchanged (110) — no testable logic, only assets/build script.
- Bumped `crossbeam-epoch` 0.9.18→0.9.20 (dev-dependency only, pulled in via
  `criterion`'s `rayon` chain for benches) to fix RUSTSEC-2026-0204, a
  pre-existing advisory unrelated to the logo work that was tripping the CI
  Security Audit check.

## Previous: v0.6.2 — CI mirrors release's target matrix

New in v0.6.2:
- `ci.yml`'s `lints` and `test` matrices gained `ubuntu-24.04-arm` (the same
  native runner `release.yml` uses for `linux-aarch64`), and a new
  `cross-build-check` job build-checks the `windows-aarch64`
  (`aarch64-pc-windows-msvc`) cross-compile target on every PR. Motivation:
  the `v0.6.0` release build failure (`libutempter` link error on
  `linux-aarch64`, fixed in v0.6.1) only surfaced when the release tag was
  pushed, because CI never exercised that runner/architecture. CI's build
  matrix should mirror release's target matrix so architecture-specific
  breakage is caught at the PR stage, not at tag time. `windows-aarch64` can
  only be build-checked (not test-run) since no ARM64 Windows CI runner
  exists to execute the resulting binary.

## Previous: v0.6.1 — fix release build on linux-aarch64

Previously in v0.6.1:
- The `v0.6.0` tag's release build failed on the new `linux-aarch64` target:
  `build.rs` only checked the `x86_64-linux-gnu` multiarch path when locating
  `libutempter`, so it silently skipped linking on `aarch64-linux-gnu` and the
  link failed with `undefined reference to utempter_add_record`. `build.rs`
  now scans all `/usr/lib/*/` multiarch directories instead of hardcoding one
  triplet. CI and release workflows also now explicitly
  `apt-get install libutempter0` on Linux runners rather than relying on it
  being preinstalled on the runner image.
- Release build matrix now uses `fail-fast: false` so one target's failure
  doesn't cancel/hide the results of the others.
- The `v0.6.0` tag was never published as a release (the build failed before
  the release job ran); `v0.6.1` supersedes it.

## Previous: v0.6.0 — Windows support for the etr client

Previously in v0.6.0:
- `etr` (the client) now builds and runs on Windows: interactive PTY sessions
  (via `crossterm` raw mode + `portable-pty`/ConPTY) and `-L`/`-R` TCP/UDP port
  forwarding both work. Verified live against a real Unix `etrs` host — remote
  command execution and `-L` local port forwarding both confirmed working from
  a native Windows console (PowerShell/conhost; Git Bash/mintty does not
  present a real Win32 console to `crossterm`, so raw-mode output does not
  render there).
- Terminal resize: Windows has no `SIGWINCH`, so the client polls
  `crossterm::terminal::size()` every 250 ms instead of waiting on a signal.
- X11 forwarding (`-X`/`-Y`) is out of scope for Windows (no Unix domain
  sockets) and is rejected at startup with a clear error rather than failing
  to build.
- `etrs` (the server) remains Unix-only by design — it daemonizes itself via
  `fork`/`setsid` and has no Windows equivalent — but the crate now builds on
  Windows: `etrs`'s CLI parsing and `--completions` still work, and attempting
  to actually run a session prints a clear error instead of failing to
  compile. Run `etrs` on the remote Unix host and connect with the Windows
  `etr` client over SSH.
- Fixed a real (pre-existing) portability bug in `src/login.rs`: the non-Linux
  stub used `std::os::unix::io::RawFd`, which doesn't exist on Windows even
  though the doc comment claimed the stub covered "other platforms".
- CI (`.github/workflows/ci.yml`): the lints job now also runs on
  `windows-latest` (in addition to `ubuntu-latest`) so clippy/fmt cover the
  `#[cfg(windows)]`/`#[cfg(not(unix))]` code paths, and the test matrix gained
  `windows-latest` alongside `ubuntu-latest`/`macos-latest`.
- Release (`.github/workflows/release.yml`): now also builds `linux-aarch64`
  (native `ubuntu-24.04-arm` runner, both `etr`+`etrs`), `windows-x86_64`
  (`etr` client only), and `windows-aarch64` (`etr` client only,
  cross-compiled to `aarch64-pc-windows-msvc` from the x86_64 Windows
  runner). Windows release assets ship the client only since `etrs` doesn't
  run there.
- Test count: 110 (unchanged — no new tests added; existing suite verified to
  still pass on Windows).

## Previous: v0.5.5 — remove GEMINI.md, update exclude list

Previously in v0.5.5:
- Removed redundant `GEMINI.md` file since the `agy` CLI reads `AGENTS.md` directly.
- Updated `exclude` list in `Cargo.toml` to remove `GEMINI.md`.

## Previous: v0.5.4 — fix CLAUDE.md path, require reading ~/AGENTS.md

Previously in v0.5.4:
- `CLAUDE.md` no longer hardcodes an absolute path to `AGENTS.md` (was broken on any
  clone not located at exactly `~/git/etr`); now a relative link.
- `AGENTS.md` Portable Core gained a `0. Global Mandates` item requiring agents to
  read `~/AGENTS.md` (global, cross-repo mandates) before doing anything else.

## Previous: v0.5.3 — merge AGENTS.md with retch, add just pr/merge-pr gate

Previously in v0.5.3:
- `AGENTS.md` restructured into a Portable Core (shared, kept in sync with `retch`'s
  AGENTS.md) plus a Part 2 project-specific section. Added WIP.md cross-machine
  handoff workflow and branch-cleanup rule, adopted from `retch`.
- Added `just pr` (automated Pre-PR gate: branch check, version-bump check, NOTES.md
  header check, man page build, Cargo.lock check, fmt/clippy, tests, manual checklist)
  and `just merge-pr` (squash-merge, reset WIP.md) recipes, mirroring `retch`'s Justfile.
- Added `scripts/reset_wip.py` and gitignored `WIP.md`.

## Previous: v0.5.2 — switch man page tooling from pandoc to mandown

Previously in v0.5.2:
- Man page build tooling replaced: `pandoc` → `mandown` (`cargo install mandown`).
  `just man` now invokes `mandown man/etr.1.md ETR 1` and patches the `.TH` line via
  sed to embed the version and "User Commands" header, matching prior pandoc output.
- YAML front matter removed from `man/etr.1.md` and `man/etrs.1.md` (mandown takes
  title and section as CLI args, not from front matter).
- `CONTRIBUTING.md` and `AGENTS.md` §4.5 updated to reference mandown.

## Previous: v0.5.1 — X11 forwarding support with Wayland/Niri fixes

New in v0.5.1:
- Wayland/Niri Compatibility: Correctly handles Wayland compositors (like `niri`) where local X11 authentication cookies are absent. The server dynamically negotiates and rewrites setup blocks to specify no-authentication, avoiding hangs.
- Robust xauth State Handling: Skips client cookie verification on the server side when the server host is missing `xauth` or command execution fails.
- Reconstructed X11 Setup Blocks: Dynamically rebuilds setup blocks (injecting `MIT-MAGIC-COOKIE-1` when client cookies are present) preserving endianness.

New in v0.5.0:
- Secure X11 Forwarding: Client accepts `-X`/`-Y` flags, extracts X11 display details and the local cookie (via `xauth`), and passes them during bootstrap.
- Dynamic Display Allocation: Server dynamically allocates a free display `$D` (checking unix socket `/tmp/.X11-unix/X$D` and loopback TCP ports `6000+$D`), spawns listeners on both Unix and TCP, and translates fake cookies back to real cookies.
- Automated Cleanup: Sockets and `xauth` cookies on the server are cleaned up via RAII/Drop when the session exits.
- Configuration Options: Client configuration now supports `x11` and `x11_trusted` flags under the `[client]` section in `config.toml`.

## Previous: v0.4.24 — Nushell zero-config completions autoloading

New in v0.4.24:
- Changed the default Nushell completions directory (`NU_COMP`) in the `justfile` to `~/.config/nushell/autoload` (respecting `XDG_CONFIG_HOME` if set) to leverage Nushell's native autoload paths.
- Naming of installed Nushell completions updated to `50etr-completions.nu` and `50etrs-completions.nu` for order management and clarity.
- `install-completions` print instructions updated to denote auto-loaded state rather than manual sourcing.

## Previous: v0.4.23 — resolve GitHub CI warnings (Node 20 deprecations)

New in v0.4.23:
- Upgraded GitHub Actions workflow versions to resolve Node.js 20 deprecation warnings.
- Upgraded `actions/checkout` to `@v6` across all workflows.
- Upgraded `actions/upload-artifact` to `@v7` and `actions/download-artifact` to `@v8` in `release.yml`.
- Upgraded `softprops/action-gh-release` to `@v3` in `release.yml`.
- Replaced the deprecated `rustsec/audit-check` action with the actively maintained `actions-rust-lang/audit@v1` in `ci.yml`.

## Previous: v0.4.22 — remote command support

New in v0.4.22:
- `etr host [command [args...]]`: optional trailing arguments run a remote
  command under the PTY instead of an interactive shell.
  Multiple words are joined with spaces and passed to `$SHELL -c`, so shell
  metacharacters (pipes, redirects) work and full-screen TUI programs like
  `btop` and `distrobox` work correctly.  The session ends when the command
  exits.  Example: `etr host 'distrobox -- btop'`.
- Bootstrap protocol: client writes `ETRCMD:<command>` as an extra line after
  env vars; old servers ignore it (no `=` → silently skipped).
- **Bug fixes** (same version, follow-up commits):
  - `etrs`: use `$SHELL -c` instead of `sh -c` so the command runs with the
    user's PATH — fixes commands only available via `~/.local/bin` (e.g. distrobox).
  - `etr`: don't enter the reconnect loop when running a remote command; exit
    with a clear error instead. Prevents the raw-mode hang where the server exits
    (command not found or immediate exit) before the client connects, leaving etr
    stuck in raw mode with Ctrl-C disabled.
  - `etrs`: when the command exits before any client connects, wait up to 1 s
    for a pending QUIC connection instead of immediately dropping the endpoint.
    The client then gets a clean Disconnect rather than a 15-second QUIC timeout.
  - `handle_connection`: if `shell_exit_rx` is already true when the connection
    arrives, immediately queue a Disconnect so it is delivered as soon as the
    PTY stream is established — covers the race where the command exits between
    QUIC accept and the first PTY exchange.
- `just e2e-cmd-local`: end-to-end test — runs a sentinel-echo command through
  a live session, checks the output appears, verifies etr exits cleanly when
  the command finishes, and (Part 2) verifies etr exits within 20 s after a
  fast-exiting command (`true`) instead of hanging forever.
- Test count: 98 → 103 (3 new `etr` CLI tests, 2 new `etrs` parse tests).

## Previous: v0.4.21 — vibe-coded disclosure in README

New in v0.4.21:
- Added a "Vibe coded" section to README.md disclosing that the project is
  entirely AI-generated (Claude and Gemini) and welcoming real programmers
  to review and contribute.

## Previous: v0.4.20 — patch quinn-proto memory exhaustion vuln

New in v0.4.20:
- `quinn` 0.11.9→0.11.11, `quinn-proto` 0.11.14→0.11.15: fixes
  RUSTSEC-2026-0185 (remote memory exhaustion via unbounded out-of-order
  stream reassembly, severity 7.5 high, published 2026-06-22).

## Previous: v0.4.19 — bump major deps; improve docs and test coverage

New in v0.4.19:
- `rand` 0.8→0.9: updated call sites in `src/bin/etr.rs` — `thread_rng()` → `rng()`,
  `Rng::gen()` → `rand::random()`, `distributions::Alphanumeric` → `distr::Alphanumeric`.
- `criterion` 0.5→0.8: no code changes required; bench suite passes.
- `clap_complete_nushell` 0.1→4.6: no code changes required.
- Added `///` doc comments to `Config` struct, `config_path()`, `StreamOpen.stream_id`,
  `StreamOpen.stream_type`, and the `Payload` enum.
- `login.rs`: added 3 tests (record_login/record_logout with invalid fd — no-panic check).
- `quic.rs`: added 3 tests — `read_tag` round-trip, oversized `read_msg` rejection (>4 MB),
  oversized `read_pty_chunk` rejection (>1 MB).
- `config.rs`: added malformed-TOML fallback test.
- `forward.rs`: added 6 `split_ignoring_brackets` edge-case tests (IPv6 host, bind+IPv6,
  no colon, empty, trailing colon).
- Test count: 78 → 98.

## Previous: v0.4.18 — fix stress-local pump connect race

New in v0.4.18:
- Fixed stress-local pump connect race: replaced the fixed `sleep 1.5` before `-R`
  pumps with a `wait_tcp_ready` bash function that polls `/dev/tcp/127.0.0.1/PORT`
  every 200ms (up to 15s) before starting each TCP pump. The `-L` pump now also
  probes its port rather than assuming the listener is immediately ready.
- `tcp_connect_with_retry` in the stress tool no longer panics on timeout; it prints
  `TCP sent=0 recv=0 elapsed=0.001` to stdout so the output file is never empty and
  the failure is visible in the throughput report rather than silently absent.
- Fixed stress_tool echo servers surviving SIGTERM: the custom SIGTERM handler (which
  sets STOP=true instead of terminating) was installed for all subcommands. Echo servers
  never check STOP so they ran indefinitely, causing "Address already in use" on the
  next run. The handler is now only installed for pump subcommands; echo servers use
  the default SIGTERM behaviour (immediate termination). Added `pkill -x stress_tool`
  to both the cleanup trap and the pre-run stale-process sweep.

## Previous: v0.4.17 — bump dirs and toml
- Bumped `dirs` 5→6, `toml` 0.8→1. No code changes required; both APIs were compatible.

## Previous: v0.4.16 — bump minor dependencies
- Bumped `crossterm` 0.27→0.29, `nix` 0.29→0.31, `prost` 0.13→0.14.
- `nix` 0.31 removed `dup2(RawFd, RawFd)`; replaced with the new `dup2_stdin` /
  `dup2_stdout` / `dup2_stderr` helpers in `detach_stdio` (`src/bin/etrs.rs`).

## Previous: v0.4.15 — prune old GitHub releases
- Release workflow now prunes releases beyond the 20 most recent after each publish.
  Uses `gh release list --limit 1000 | .[20:]` piped to `gh release delete --cleanup-tag`
  in a `prune` job that runs after the `release` job. No new permissions needed —
  `contents: write` was already set at the workflow level.

## Previous: v0.4.14 — install-completions just recipe

New in v0.4.14:
- `just install-completions`: generates and installs shell completions for `etr` and `etrs`
  into the correct XDG directories for all six supported shells (bash, zsh, fish, elvish,
  nushell, powershell). Depends on `build` (debug binaries). Shells that require manual
  sourcing (elvish, nushell, powershell) print instructions at the end of the run.
- Six new justfile variables (`BASH_COMP`, `ZSH_COMP`, `FISH_COMP`, `ELVISH_COMP`,
  `NU_COMP`, `PS_COMP`) follow the same `${XDG_…:-default}` pattern as `MAN_DIR`.
  zsh uses `$XDG_DATA_HOME/zsh/site-functions` (in zsh's compiled-in default `$fpath`);
  `$XDG_DATA_HOME/zsh/completions` is NOT in the default and requires user configuration.

## Previous: v0.4.13 — config generation and merge

New in v0.4.13:
- `etr --generate-config`: prints a fully-commented default `config.toml` to stdout.
- `etr --write-config [PATH]`: writes the default config to `~/.config/etr/config.toml` (or a custom path), creating parent directories as needed.
- `etr --merge-config`: adds any missing config keys (as commented-out blocks) to the existing config file without touching keys already present. Idempotent. Missing keys are inserted inside their existing section header rather than appended with a duplicate header, so the result is always valid TOML.
- `config.rs`: new `pub const DEFAULT_CONFIG`, `pub fn merge_defaults`, 10 new unit tests.
- `Configuration` wiki page: rewritten to document every CLI flag and every config key with types, defaults, and examples.
- Test count: 85 (up from 78).

## Previous: v0.4.12 — issue templates

New in v0.4.12:
- Added `.github/ISSUE_TEMPLATE/bug_report.md` and `feature_request.md`, completing GitHub community standards.

## Previous: v0.4.11 — community health files

New in v0.4.11:
- Added `CODE_OF_CONDUCT.md` (Contributor Covenant v2.1; enforcement contact via GitHub issues or @l1a).
- Added `CONTRIBUTING.md` with bug reporting, PR, and dev-setup guidance.
- Added `.github/pull_request_template.md` aligned with the pre-PR checklist in AGENTS.md.
- Set repo description and wiki homepage URL on GitHub.

New in v0.4.10:
- `src/login.rs`: module doc converted from `//` to `//!`; `///` doc comments added to
  `record_login` and `record_logout`; `// SAFETY:` comments added to both `unsafe` FFI
  call sites.
- `just e2e-env-local`: new end-to-end test covering `--env KEY=VALUE` (explicit set)
  and `--env KEY` (bare forward from local environment) through a live session.
- `just e2e-udp-concurrent` + `scripts/stress/udp_concurrent_senders.py`: regression
  test for the v0.4.9 per-sender UDP routing fix.  Two concurrent senders each assert
  they receive their own echo reply, not the other sender's.
- AGENTS.md: added unconditional-step table and anti-rationalization language to prevent
  future agents from skipping the version bump (4.10) or man page build (4.5).

New in v0.4.9:
- UDP forwarding (`-L` and `-R`) now correctly handles multiple concurrent senders.
  Each unique source address gets its own ephemeral UDP socket on the forwarding side,
  so replies are routed back to the correct sender regardless of interleaving order.
  Idle sender sockets are evicted after 30 s.  Removes the last-sender-wins limitation
  for concurrent DNS/STUN/game-protocol clients on the same forwarded port.

## Current state v0.4.7 — meaningful errors on server exit + hang/SIGTERM fixes

The full round-trip works: `etr <host>` on the client, SSH bootstrap that starts
`etrs` on the fly, QUIC connection with cert pinning, PTY session, keepalives,
reconnecting after drops, `-L` local port forwarding, and `-R` remote port forwarding (both TCP and UDP).
Tested on Linux and macOS (aarch64).  Published to crates.io; `cargo install etr` installs both binaries.

New in v0.4.7:
- When the server exits unexpectedly (crash, reboot), `etr` now prints `[etr] Connection lost.`
  unconditionally (previously the message was only shown with `-v`).
- The reconnect-in-progress message `[etr] Reconnecting to <addr>...  (Enter ~. to force-quit)`
  is now always visible, not hidden behind `-v`.
- Bootstrap errors are printed as `[etr] <message>` instead of the cryptic Rust
  `Error: Custom { kind: Other, error: "..." }` Debug format.
- Internal error string "PTY stream closed" replaced with "server connection dropped" so that
  the dropped-session reason shown at `-v` is user-facing.
- **QUIC idle timeout (30 s) and keepalive (10 s)** added to both client and server transport
  config.  Previously, if the server vanished (crash, reboot, network partition) the client
  would hang indefinitely; now the connection is declared dead within 30 s and the client
  moves to the reconnect loop automatically.
- **`etrs` SIGTERM/SIGHUP during active session**: previously the server only checked for
  signals while waiting for the next reconnect; if a signal arrived while a session was
  active it was silently dropped and the server continued running.  Now a second signal
  listener pair wraps `handle_connection` in a `tokio::select!` — the connection is closed
  cleanly, utmp logout is recorded, and the server exits.

New in v0.4.6:
- `etrs` now spawns the shell as a proper login shell (argv[0]=`-zsh`) via
  `CommandBuilder::new_default_prog()`, so `.zprofile`/`.zlogin` are sourced, matching SSH.
- `ETR_CONNECTION=1` and `ETR_VERSION` are set in the remote shell environment.
- `etr` supports a `~.` escape sequence (SSH-style, at line-start) to force-disconnect when the server is unresponsive.
- Server reconnect timeout is configurable via `--reconnect-timeout`, `ETR_SERVER_NETWORK_TMOUT`
  env var, or `[server] reconnect_timeout` in the config file (default: 1800 s).

---

## Architecture

### Two binaries

| Binary | Role |
|--------|------|
| `etrs` | Per-session server — started by `etr` via SSH, forks after binding a QUIC port, exits on clean disconnect |
| `etr`  | Client — SSH bootstrap, QUIC connection loop, raw-mode terminal |

### Connection lifecycle

```
[client machine]                        [server machine]
  etr
   │
   ├─ 1. generate session_id + passkey (random)
   │
   ├─ 2. ssh target "etrs"
   │       stdin: session_id_hex/passkey/term
   │                                         │
   │                                        etrs
   │                                         │ generates self-signed TLS cert
   │                                         │ binds QUIC port 0 (OS assigns)
   │                                         │ prints "PORT <n> CERT <cert_hex>"
   │                                         │ forks → parent exits (SSH returns)
   │                                         │ child: detaches stdio, runs session
   │◄── reads "PORT <n> CERT <cert_hex>" ───┘
   │
   └─ 3. QUIC connect (TLS 1.3, pinned cert) ►  etrs child
                                               ◄──────────────
         QUIC stream 0x01 (control):
           client → SessionOpen{session_id, passkey, last_received_seq}
           server → SessionAccept{last_received_seq}

         QUIC stream 0x02 (PTY):
           client → stdin keypresses [seq][len][data]
           server → PTY output [seq][len][data]

         QUIC stream 0x03 (forward, one per TCP conn or UDP spec):
           client → StreamOpen header + raw bytes
           server → raw bytes

   (on clean Disconnect: etrs child exits; no daemon remains)
```

### SSH bootstrap detail

`etr` runs `ssh -p <ssh_port> <target> etrs` and writes
`session_id_hex/passkey/term\n` to stdin.  `etrs` generates an ephemeral
self-signed TLS certificate, binds a random QUIC port, and prints
`PORT <n> CERT <cert_der_hex>` to stdout (which `etr` reads), then forks:

- **Parent**: exits immediately, allowing the SSH connection to close cleanly.
- **Child**: calls `setsid()`, redirects stdio to `/dev/null` (stderr → session log),
  then builds a Tokio runtime and runs the session loop.

`etr` uses the cert DER (hex-encoded) received over SSH to pin the server's
TLS certificate — analogous to SSH host-key pinning.  No CA is required.

### Reconnect

The client detects a dropped connection when any of its per-connection tasks
fail (QUIC stream errors / connection close).  It loops: re-connect via QUIC,
send SessionOpen with `last_received_seq` watermarks, receive SessionAccept,
replay unacknowledged outbound data (stdin history), resume the PTY stream.
The server keeps session state (send history, PTY) alive across reconnects
for up to 30 minutes.  A new QUIC source address is fine — the session is
keyed by `session_id` + `passkey`, not the peer address.  On clean disconnect,
`etrs` exits immediately.

---

## Transport: QUIC (quinn 0.11)

QUIC provides reliable, ordered, multiplexed streams with congestion control
and TLS 1.3 — solving the packet-loss / reordering problem that the prior UDP
design had.

### What QUIC replaces

| Old (UDP)                      | New (QUIC)                                      |
|--------------------------------|-------------------------------------------------|
| Custom KEM/AEAD crypto         | TLS 1.3 (X25519 + AES-256-GCM-SHA384 / ChaCha) |
| PacketHeader (26 bytes)        | QUIC stream framing (built-in)                  |
| ClientHello / ServerHello      | SessionOpen / SessionAccept on control stream   |
| Per-packet AEAD encryption     | TLS record layer (built-in)                     |
| Gap detection / discard        | Reliable ordered delivery (built-in)            |
| `StreamData` + seq-num routing | Separate QUIC bidi stream per forward           |

### Session persistence

`send_history`, `record_send`, `replay_from`, `last_received_seq` are still
needed because QUIC does not replay application data on new connections.
The seq numbers embedded in PTY stream chunks (`[8-byte seq][4-byte len][data]`)
let the server know exactly what to replay after a reconnect.

**Memory bounding**: `send_history` is capped at **4 MB per stream** (byte-based).
Entries are evicted oldest-first when the cap is exceeded, independent of
heartbeat-ack trimming.  Heartbeat messages (`Heartbeat.last_received_seq`) piggyback
the receiver's watermark every 5 s so acknowledged entries are also trimmed
continuously — in normal use the buffer stays near zero.

### PQC note

The bespoke ML-KEM layer is retired.  Standard TLS 1.3 uses X25519 ECDH.
Post-quantum key exchange can be re-added later via `rustls-post-quantum`
(X25519MLKEM768 hybrid, in TLS standardisation pipeline).

---

## Wire protocol

### QUIC stream tags (first byte on every client-opened bidi stream)

| Tag  | Stream  | Purpose                                  |
|------|---------|------------------------------------------|
| 0x01 | Control | Session handshake + heartbeats + resize  |
| 0x02 | PTY     | Terminal I/O (raw, seq-numbered chunks)  |
| 0x03 | Forward | Port-forward (StreamOpen header + bytes) |

### Control stream (0x01)

```
client → server: [4-byte len][SessionOpen proto]
server → client: [4-byte len][SessionAccept proto]
then (both directions): [4-byte len][Envelope proto]
    Envelope contains one of: Heartbeat, TerminalResize, Disconnect
```

### PTY stream (0x02)

```
each chunk (both directions): [8-byte seq BE][4-byte len][data]
server → client: PTY output
client → server: stdin keypresses
```

### Forward stream (0x03, TCP)

```
client → server header: [4-byte len][StreamOpen proto]
then raw bytes both directions (one QUIC stream per TCP connection)
```

### Forward stream (0x03, UDP)

```
client → server header: [4-byte len][StreamOpen proto]
then: [4-byte len][UdpDatagram proto] in both directions
    UdpDatagram embeds peer_addr + peer_port for last-sender routing
```

---

## Verbosity / diagnostics

Both binaries support `-v` / `-vv` / `-vvv` (SSH-style count):

| Level | `etrs` shows | `etr` shows |
|-------|-------------|-------------|
| `-v`  | session lifecycle (connect, disconnect, timeout) | connection events |
| `-vv` | QUIC details, session ID | QUIC details, session ID |
| `-vvv` | stream trace | stream trace |

**Client log file**: when `etr` is run interactively with `-v` or higher, logs go to
`$XDG_STATE_HOME/etr/etr.log` (default: `~/.local/state/etr/etr.log`) rather than
stderr, to avoid corrupting the raw-mode terminal display.

**Server log file**: `etrs` writes to `$XDG_STATE_HOME/etr/etrs.log` (default:
`~/.local/state/etr/etrs.log`) after forking.  Watch with `just log`.

---

## Configuration

A TOML config file is loaded from `$XDG_CONFIG_HOME/etr/config.toml`
(default: `~/.config/etr/config.toml`).  All fields are optional.

```toml
[client]
# Default SSH port (default: 22)
ssh_port = 22

# Path to etrs on remote hosts (default: "etrs", relies on PATH)
server_path = "/usr/local/bin/etrs"
```

---

## Ports and paths

| Resource | Default | Override |
|----------|---------|----------|
| QUIC data port | OS-assigned (random high port) | `etrs -p PORT` |
| SSH port | 22 | `-s PORT` or config `ssh_port` |
| etrs binary path | `etrs` (PATH) | `--server-path` or config `server_path` |
| Server log | `~/.local/state/etr/etrs.log` | `etrs --log-path PATH`, `etr --server-log-path PATH`, or config `server_log_path` |
| Client log | `~/.local/state/etr/etr.log` | `etr --log-path PATH` or config `log_path` |
| Server bind address | `[::]` (dual-stack) | `etrs -b ADDR` |

IPv6 is fully supported.

---

## Building and installing

```bash
# Development build
cargo build

# Install (release): both binaries, man pages and completions for six shells
just install

# Install a released tag instead -- binaries, completions and man pages all from that tag
just install-tag 0.7.3

# Code quality gate — run before every commit
just check            # cargo fmt --check + cargo clippy -D warnings (also runs standard-check)
just test             # cargo test (112 tests)
```

---

## Running

```bash
# No pre-started server needed — etr starts etrs on the fly via SSH.

# On the client
etr user@host             # standard connect
etr localhost             # localhost testing (SSH to localhost must be configured)
etr -vvv host             # verbose — shown on stderr before session, then logged to
                          #   ~/.local/state/etr/etr.log during raw-mode session

# Server logs land in ~/.local/state/etr/etrs.log on the server.

# Prerequisites for localhost testing
ssh-copy-id localhost     # or append ~/.ssh/id_*.pub to ~/.ssh/authorized_keys
just check-tools          # verifies tmux, ssh, passwordless localhost SSH

# Full automated end-to-end test (happy path + reconnect)
just e2e-local

# Memory/throughput stress test (1 PTY + 2 -L forward streams, all directions)
just stress-local
```

---

## Product vision

### Mode 1 — Persistent reconnecting shell (like mosh)

The primary use case.  `etr user@host` works with **no pre-configuration on the
server** — analogous to how mosh works.  The client SSHes to the server, `etrs` is
started on the fly, binds a random QUIC port, forks, and the SSH connection closes.
`etr` then connects to the QUIC port for the persistent session.

**Current state**: fully implemented.

### Mode 2 — Persistent port forwarding (like `ssh -L`/`-R`)

A one-shot invocation that opens a forwarded socket and keeps it alive across network
interruptions, without a PTY session.  Example:

```bash
etr -L 5432:db-host:5432 user@jumphost    # local port → remote (TCP)
etr -L 5353:8.8.8.8:53/udp user@jumphost # UDP forwarding
```

**Current state**: `-L [bind_address:]local_port:remote_host:remote_port[/tcp|/udp]` is implemented for
both TCP and UDP, running concurrently alongside the PTY session.  TCP opens one QUIC
stream per connection; UDP uses one shared QUIC stream per `-L` spec with last-sender reply routing.
By default, local listeners are bound to both `127.0.0.1` and `[::1]` loopbacks. If `-g`/`--gateway-ports` is specified,
they are bound to wildcard addresses (`0.0.0.0` and `[::]`). Specific bind addresses can be set in the spec.
Runs without a PTY session if no terminal is attached.
`-R [bind_address:]remote_port:local_host:local_port[/tcp|/udp]` is implemented for both TCP and UDP.
By default, remote listeners are bound to both `127.0.0.1` and `[::1]` loopbacks on the target machine, but explicit bind addresses (e.g. `*` or `0.0.0.0`) can be specified to allow external hosts to connect.

---

## Known gaps / next steps

- **Clean shell `exit` reconnecting instead of quitting (observed once,
  unreproduced — mechanism unconfirmed)**: a single Windows→WSL observation
  showed `etr` entering the reconnect loop after the remote shell exited, rather
  than quitting on a clean `Disconnect`. A follow-up investigation could **not
  reproduce** it in 24 controlled trials (including a 300 ms delay injected to
  widen the suspected race window). The initial teardown-race hypothesis was
  **disproven**: `pty_writer_task` does not finish on PTY-EOF (the PTY feeder in
  `handle_connection` keeps the channel sender alive), so `ctrl_writer_task` is
  not aborted before delivering the `Disconnect`; on clean exit the connection
  stays alive and the client exits cleanly. If this recurs, it is more likely a
  real-network timing artifact (the live connection dropping mid-delivery and the
  reconnect missing `etrs`'s 1 s pending-client window) — a different mechanism.
  Do not attempt a fix without first capturing `etr -vvv` and `etrs` logs from an
  actual occurrence to identify the real cause. Note the terminal is restored
  correctly on `~.` regardless (client-side, v0.6.5).
- ~~**`just` recipes unusable from native Windows shells**~~ **Done in v0.7.1** (this entry
  was left un-struck until v0.7.4 — see that section). The install-family recipes were
  `#!/usr/bin/env bash` shebang recipes, so on Windows `just` tried to translate the
  interpreter path with `cygpath` and every one failed before running a line from
  PowerShell/nushell. They are now plain recipes driving two vendored Python helpers
  (`scripts/install_completions.py`, `scripts/install_man.py`), which need no `sh`, no
  `cygpath`, no coreutils and nothing from Git's `usrin` — which is precisely the
  "plain (non-shebang) recipes" fix this gap asked for.
- ~~**Remote command truncated with redirected stdin**~~ **Done in v0.7.5**: the client no
  longer ends the session when stdin hits EOF while a remote command is running. Note the
  prescription recorded here — "half-closing the stdin path (stop sending) while continuing
  to drain PTY output" — turned out to be **necessary but not sufficient**, twice over: the
  sender must be kept alive (dropping it finishes the QUIC stream and the server ends the
  session), and `VEOF` must be relayed in-band (a PTY cannot be half-closed, so a
  stdin-reading command such as `cat` otherwise hangs). See the v0.7.5 section.
- ~~**`etr` panics when there is no controlling terminal**~~ **Done in v0.7.6**: the
  decision this entry asked for was taken — degrade for a remote command, refuse with a
  diagnostic for an interactive session — because the right behaviour does differ between
  the two. See the v0.7.6 section, including the `RAW_EVER_ENABLED` guard that keeps the
  degraded path's output free of VT escapes.
- ~~**`utmp`/`wtmp` registration**~~ **Done**: `etrs` writes `USER_PROCESS` to utmp
  and wtmp on connect, and `DEAD_PROCESS` on clean shell exit, via `libutempter`
  (`src/login.rs`).  `libutempter` delegates to the setgid-utmp helper
  `/usr/libexec/utempter/utempter` so `etrs` needs no special privileges.
  Sessions appear in `last`; `who`/`w` read from systemd-logind on modern Fedora
  and do not show utmp-only sessions.  Non-Linux builds get no-op stubs.
- ~~**Benchmarking**~~ **Done**: Criterion benchmark suite implemented in `benches/session_bench.rs` measuring certificate generation, QUIC connection handshake latency, PTY round-trip latency (100b), and throughput (64kb).
- ~~**Mode 2 — `-R` remote forwarding**~~ **Done**: Both TCP and UDP remote port forwarding are supported using the `-R` CLI flag.
- ~~**Client-side environment variable forwarding**~~ **Done**: `--env KEY=VALUE` (repeatable) sets arbitrary environment variables in the remote shell. `--env KEY` (no `=`) forwards from the local environment. Config file equivalent: `[client] env = ["KEY=VALUE", "KEY2"]`.
- ~~**UDP reply routing**~~ **Done**: Each unique local UDP sender (`peer_addr:peer_port`) now gets its own ephemeral socket on the server (`-L`) and client (`-R`), so replies from the remote target are routed back to the correct sender regardless of interleaving. Idle sender sockets are evicted after 30 s. This removes the last-sender-wins limitation for concurrent DNS/STUN/game-protocol clients.
- ~~**`--env` e2e test**~~ **Done**: `just e2e-env-local` tests both `--env KEY=VALUE` (explicit set) and `--env KEY` (bare forward from local env) end-to-end through a live `etr localhost` session.
- ~~**Concurrent UDP senders regression test**~~ **Done**: `just e2e-udp-concurrent` sends interleaved datagrams from two independent sockets through `-L` UDP forwarding and asserts each socket receives its own reply. Regression coverage for the v0.4.9 per-sender routing fix.
- ~~**X11 forwarding**~~ **Done**: X11 forwarding (`-X` and `-Y`) is implemented with dynamic display allocation, xauth cookie spoofing/rewriting, and automatic socket/cookie cleanup. Wayland forwarding and Wayland compositor proxying are out of scope.
- **PQC key exchange**: ML-KEM was retired with the QUIC migration.  Can be re-added
  via `rustls-post-quantum` (X25519MLKEM768 hybrid) once it stabilises.
- **macOS**: fully tested and working.  PTY session, reconnect, and port forwarding
  all pass.  Test harness fixes applied: `ps -o ppid=` replaces Linux-only
  `/proc/$$/status`; reconnect test stops the etrs daemon (not the etr client)
  because stopping a PTY-attached process on macOS triggers a SIGHUP that kills it.
- ~~**Windows client support**~~ **Done**: `etr` (client only) builds, and interactive
  sessions + `-L`/`-R` forwarding are verified working live against a real Unix
  `etrs` host. X11 forwarding is unsupported on Windows (rejected at startup).
  `etrs` (server) is still Unix-only by design (fork/setsid daemonization has no
  Windows equivalent) — it now builds on Windows for CLI/`--completions`
  purposes only and errors clearly if you try to actually run a session with it.
- ~~**Shell completions for `etrs`**~~ **Done**: `etrs --completions <shell>` generates completions for bash, zsh, fish, elvish, PowerShell, and nushell via `clap_complete`/`clap_complete_nushell`, mirroring the existing `etr --completions` support.
- ~~**utmp address field incorrect for IPv4 connections**~~ **Done**: `peer.ip().to_canonical()` in `src/bin/etrs.rs` unwraps IPv4-mapped IPv6 addresses (`::ffff:127.0.0.1` → `127.0.0.1`) before passing to `utempter_add_record`, so `last` and friends see a plain IPv4 dotted-quad.
- ~~**Stale utmp entry on unclean exit**~~ **Done**: `etrs` now listens for SIGTERM and SIGHUP in the reconnect loop and calls `record_logout` before exiting, so `who`/`last` entries are cleaned up even when the session is killed rather than ended by the shell exiting.
- **Throughput**: TCP relay buffer raised 8 KB → 256 KB; QUIC flow-control windows
  raised to 4 MB per stream / 32 MB per connection; `TCP_NODELAY` on all forwarded TCP
  connections.  Stress-local (echo test) measures ~320 Mb/s TCP, but this is an
  echo-path number — iperf3 one-direction through the tunnel measures **~2.1 Gbits/s**
  with an optimized build.  Debug-build overhead accounts for an additional ~2.6× gap
  (stress-local uses a debug build).  The goal of "within an order of magnitude of
  iperf3" (~12 Gbits/s) is still ~5–6× away.
  Profiling (`samply` attached to etrs) shows AES-GCM decryption is NOT the bottleneck
  (<1% of samples); the dominant overhead is Quinn's per-packet state machine (stream
  delivery, ACK tracking, timer heap).  `read_chunk` (zero-copy from Quinn buffers) was
  tested but regressed throughput from 2.1 → 1.8 Gbits/s because it produces one tiny
  `write_all` per Quinn frame instead of coalescing them into our 256 KB read buffer —
  more syscalls, not fewer copies, determines throughput here.
  UDP (~9 Mb/s) is still limited by per-datagram protobuf encoding overhead.
- ~~**UDP forward target resolution should prefer IPv6 when genuinely available**~~ **Done**: `etr::forward::resolve_udp_target` (new helper in `src/forward.rs`) resolves the target, tries IPv6 candidates first, and probes routing via a no-packet UDP `connect()` call.  The first address whose routing probe succeeds is used.  Falls back to IPv4 if no IPv6 route exists.  The stress-tool UDP echo server now also binds `[::1]:port` alongside `0.0.0.0:port` so both families reach it in tests.
- ~~**GitHub release retention**~~ **Done**: the release workflow's `prune` job deletes releases beyond the 20 most recent after each publish, using `gh release delete --cleanup-tag`.
- ~~**Dependency updates (minor/safe)**~~ **Done**: `crossterm` 0.27→0.29, `nix` 0.29→0.31, `prost` 0.13→0.14.
- ~~**Dependency updates (major)**~~ **Done**: `rand` 0.8→0.9, `clap_complete_nushell` 0.1→4.6, `criterion` 0.5→0.8.
- ~~**stress-local: pump connect race**~~ **Done**: replaced fixed sleep with `wait_tcp_ready` probe; stress tool now prints zero stats instead of panicking on connect timeout.

---

## Test coverage (112 tests)

| Module | What's tested |
|--------|--------------|
| `quic` | Cert generation, server/client config, write/read Envelope framing, write/read PTY chunk framing |
| `protocol` | SessionOpen/Accept encode-decode (incl. `gateway_ports`, `reverse_forwards`, and `x11_enabled`/`x11_auth_proto`/`x11_auth_cookie` round-trip), StreamOpen/Close, Heartbeat, Disconnect, UdpDatagram |
| `session/stream` | Acknowledge edge cases, replay from 0, initial seq values |
| `session/mod` | Close/ack unknown stream, `last_received_map` semantics, collect_replays, `open_stream` idempotence |
| `bin/etrs` | CLI defaults, verbose count, custom port, subcommand parsing, hex_decode, custom --log-path override, `ETRX11` bootstrap line parsing |
| `login` | no-panic checks for record_login / record_logout with invalid fd |
| `bin/etr` | CLI defaults, port parsing, target parsing, no --cipher flag, custom --log-path and --server-log-path overrides, config fallback for log paths, terminal-restore sequences (cursor-safe modes cover mouse/paste/cursor and never move the cursor; screen reset leaves alt-screen without clearing scrollback) |
| `config` | TOML parse (full section, partial, empty), default values, `gateway_ports` / `forward` / `reverse_forward` / `x11` / `x11_trusted` config keys |
| `forward` | `-L`/`-R` spec parsing: TCP/UDP/IPv6, explicit proto, bad port, empty host, Display; bind address parsing (explicit IP, `[::1]`, wildcard `*`); `get_bind_addresses` with and without gateway flag; `resolve_udp_target`: localhost prefers IPv6, explicit IPv4, unresolvable host; `X11Display` parsing |
