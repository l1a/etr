# etr — Project Status Notes

Project state and standing knowledge for **etr**: architecture, wire protocol, current
status, known gaps, and the lessons that cost something to learn.

**What belongs here:** anything a developer or agent must keep in mind to avoid repeating a
mistake, re-deriving a decision, or losing time to a trap this project has already hit.

**What does not:** a per-release changelog. History lives in `git log` and the
[GitHub releases](https://github.com/l1a/etr/releases); the wiki carries user-facing
documentation. Until v0.10.10 this file held a full release log — ~2,060 lines of it, back
to v0.4.6 — which buried the parts that are actually load-bearing. The durable content of
those entries was distilled into **Hard-won lessons** below; the narrative was dropped
rather than migrated, because a reader who needs "what changed in v0.9.3" is better served
by `git log`.

**Keep it that way.** When a release entry would only say what changed, write a good commit
message instead. Add to this file when a change leaves behind a *rule* — something the next
person would otherwise get wrong.

## What this is

`etr` is a Rust reimplementation of [Eternal Terminal](https://eternalterminal.dev/) (`et`).
Eternal Terminal is a persistent remote shell that survives network interruptions — unlike
SSH, the session keeps running on the server and the client reconnects transparently when
the link drops.  This project uses **QUIC** (via the `quinn` crate) for the transport
layer, which provides reliable, ordered, multiplexed streams with congestion control
and TLS 1.3 built-in.

## Current state: v0.10.10

`main` carries **0.10.10**. The newest released tag is **`v0.10.6`**, live on GitHub,
crates.io, the AUR, COPR and the Homebrew tap; **0.10.7, 0.10.8, 0.10.9 and 0.10.10 are
merged and untagged**, all of them tooling or documentation.

The full round trip works on Linux and macOS: SSH bootstrap, QUIC session with pinned
certificate, PTY, keepalives, reconnect after a drop, `-L`/`-R` TCP and UDP forwarding,
X11 forwarding, `-4`/`-6` family preference. The Windows **client** works; `etrs` is
Unix-only by design. 153 tests.

What is open is listed under **Known gaps / next steps** below.

Recent work worth knowing about, beyond what `git log` says:

- **v0.10.10 — `NOTES.md` and `WIP.md` pruned to their jobs.** This file stops being a
  changelog (see the header); `WIP.md` now carries only work in flight, with its scope
  stated in its own header so it does not regrow. `AGENTS.md` §4.9 and the `just pr` manual
  checklist are updated to match. `README.md` is deliberately untouched — overview, install,
  usage and configuration is the right role for it.
- **v0.10.10 — LF is stated as the base model, and the one unguarded file now has a guard.**
  A byte-count survey found etr already clean (71 tracked text files, 0 carriage returns;
  `WIP.md` 0), so nothing needed converting. What was missing was a guard for `WIP.md`, which
  is gitignored and therefore invisible to both `.gitattributes` and `text_check.py` while
  being rewritten on every merge. New `just wip-check` (in `just check`) asserts it, and
  `scripts/reset_wip.py` now reads and writes bytes so a merge can never convert the file as
  a side effect.
- **v0.10.8 — `man-check` checks the commit, not only the worktree.** A `--amend` without
  `git add man/` can no longer ship a stale page past a green gate. HEAD is compared with
  itself, never with the worktree, so an uncommitted bump mid-work still passes.
- **v0.10.7 — `just install` no longer needs mandown.** `install-man` installs the committed
  pages instead of rebuilding them, so installing the program no longer requires the tool
  used to write its documentation.
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

# Which IP version to try first: "ipv4", "ipv6" or "auto" (default)
address_family = "auto"

[server]
# Same for etrs: forward-target resolution, and the default bind
# ("ipv4" binds 0.0.0.0; "ipv6"/"auto" bind the dual-stack [::]).
address_family = "auto"
```

Run `etr --generate-config` for the fully-commented file, or `etr --merge-config`
to add newly-introduced keys to an existing one.

---

## Ports and paths

| Resource | Default | Override |
|----------|---------|----------|
| QUIC data port | OS-assigned (random high port) | `etrs -p PORT` |
| SSH port | 22 | `-s PORT` or config `ssh_port` |
| etrs binary path | `etrs` (PATH) | `--server-path` or config `server_path` |
| Server log | `~/.local/state/etr/etrs.log` | `etrs --log-path PATH`, `etr --server-log-path PATH`, or config `server_log_path` |
| Client log | `~/.local/state/etr/etr.log` | `etr --log-path PATH` or config `log_path` |
| Server bind address | `[::]` (dual-stack) | `etrs -b ADDR`, or `0.0.0.0` under `etrs -4` |
| Address family | resolver order (IPv6-first for UDP forward targets) | `-4`/`-6` on either binary, or config `address_family` |

IPv6 is fully supported.  `-4`/`-6` express a *preference* — the other family is
still used when the requested one is absent or unroutable — which is why they are
named `--prefer-ipv4`/`--prefer-ipv6` and behave unlike `ssh -4`/`-6`.

---

## Building and installing

```bash
# Development build
cargo build

# Install (release): both binaries, man pages and completions for six shells
just install

# Install a released tag instead -- binaries, completions and man pages all from that tag.
# Since v0.9.0 the man pages are tracked, so this no longer reports them missing at the tag.
just install-tag 0.9.0

# Code quality gate — run before every commit
just check            # fmt + clippy, and: standard-check, man-check, packaging-check,
                      #   text-check, wip-check
just test             # cargo test (153 tests)

# Man pages are TRACKED (man/etr.1, man/etrs.1) because tag tarballs carry only tracked
# files and COPR/Homebrew install them from there. Re-run after every version bump.
just man
just man-check        # fails if the committed pages are stale (run by `just check`)

# Byte-level text guards. `text-check` refuses control characters and carriage returns in
# TRACKED text; `wip-check` covers WIP.md, which is gitignored and so out of its reach.
just text-check
just wip-check
```

### Packaging and publishing

```bash
just packaging-check          # offline: sentinels intact, every channel agrees (in `just check`)
just copr-render              # what COPR will be handed, without rpm tooling
just github-metadata --dry-run  # About-box description/topics vs packaging/metadata.toml

# Release: tag first (starts release.yml AND copr.yml), wait for assets, then push the rest.
git tag v0.9.0 && git push origin v0.9.0
just publish                  # crates.io -> AUR -> Homebrew; refuses unless HEAD is the tag
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
etr -6 host               # prefer IPv6 (falls back to IPv4 if there is none usable)
etr -4 host               # prefer IPv4

# Server logs land in ~/.local/state/etr/etrs.log on the server.

# Prerequisites for localhost testing
ssh-copy-id localhost     # or append ~/.ssh/id_*.pub to ~/.ssh/authorized_keys
just check-tools          # verifies tmux, ssh, passwordless localhost SSH

# Full automated end-to-end test (happy path + reconnect)
just e2e-local

# -4/-6 actually change the family connected over (not just that they parse)
just e2e-family-local

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

Live items only. **Delete a finished item rather than striking it through** — the record of
why it closed is in `git log` and the release notes, and a list of crossed-out entries is
how this section stopped being readable.

### Correctness

- **Clean shell `exit` reconnecting instead of quitting (observed once, unreproduced —
  mechanism unconfirmed).** A single Windows→WSL observation showed `etr` entering the
  reconnect loop after the remote shell exited rather than quitting on a clean `Disconnect`.
  A follow-up could **not reproduce** it in 24 controlled trials, including a 300 ms delay
  injected to widen the suspected race window. The teardown-race hypothesis was
  **disproven**: `pty_writer_task` does not finish on PTY-EOF (the PTY feeder in
  `handle_connection` keeps the channel sender alive), so `ctrl_writer_task` is not aborted
  before delivering the `Disconnect`. If it recurs it is more likely a real-network timing
  artefact — the live connection dropping mid-delivery and the reconnect missing `etrs`'s
  1 s pending-client window, a different mechanism entirely. **Do not attempt a fix without
  `etr -vvv` and `etrs` logs from an actual occurrence.** The terminal is restored correctly
  on `~.` regardless.

### Measurement

- **The real UDP knee is unmeasured.** Forwarding sustains ≥100 Mb/s at 0% loss, but that is
  the *pacer's* ceiling, not etr's: `thread::sleep` cannot pace finer than ~100 µs, so a
  requested 50 µs step lands at ~111 µs ≈ 100 Mb/s. Finding the true knee needs a busy-wait
  pacer.
- **Under concurrent multi-gigabit TCP load the UDP forwards deliver ~41 of 100 Mb/s
  offered.** UDP has no flow control and is competing with saturating TCP; whether this is
  contention or a forwarder limit is unmeasured.
- **MTU awareness, if quinn improves.** quinn caps datagrams at 1452 (`MtuDiscoveryConfig::
  upper_bound`), which is why loopback shows etr at ~14% of raw TCP. Raising the bound today
  measures **28% slower** (see Hard-won lessons), so this is a deliberate future option, not
  an open defect: if MTU discovery gets cheaper or converges faster, raising it becomes a real
  win on jumbo-frame LANs. The safe shape is conditional (a detected local/jumbo path, or an
  explicit flag), never global, and re-measured with the one-way tooling on both a standard
  and a jumbo path.

### Tooling and hygiene

- **`just check` still needs bash on this repo.** `man-check`, `packaging-check` and
  `text-check` are `#!/usr/bin/env bash` shebang recipes, so `just check` does not run on a
  default Windows PATH. retch's is shell-free end to end. Closing this means converting the
  three to Python helpers — retch's `v0.6.16` argument applied here.
- **`scripts/text_check.py` carries a claim its siblings have corrected.** Its docstring says
  a CRLF worktree is invisible because "`git status` CANNOT report it". retch corrected this
  in its v0.17.12: git reports the drift exactly once, as an ` M` with **no diff behind it**,
  and the first `git add` erases that signal while leaving every CR on disk. Narrower and
  worse than the claim here. The file is vendored to all three repos, all three bodies
  differ, and all three still declare `TEMPLATE_VERSION = 1` — so the version marker, the one
  thing that makes a vendored copy safe, cannot tell them apart. Needs its own PR, coordinated
  across the three.
- **`scripts/reset_wip.py` does not own a marked block.** It now refuses unless each of its
  two patterns matches exactly once, which closes the class; retch went further in its #259,
  regenerating only the text between its own `BEGIN`/`END` markers and refusing on duplicated
  or unpaired ones. Worth adopting if `WIP.md`'s layout ever needs to move.
- **Release**: 0.10.7 through 0.10.10 are merged and untagged. All tooling or documentation;
  reasonable to batch.

### Deliberately out of scope

- **PQC key exchange.** ML-KEM was retired with the QUIC migration. Re-addable via
  `rustls-post-quantum` (X25519MLKEM768 hybrid) once it stabilises.
- **Wayland forwarding and Wayland compositor proxying.** X11 forwarding is implemented;
  Wayland is not planned.
- **X11 forwarding on Windows.** No Unix domain sockets; rejected at startup with a clear
  error rather than failing to build.
- **`etrs` on Windows.** It daemonises via `fork`/`setsid`, which has no Windows equivalent.
  The crate builds there so `--completions` and CLI parsing work, and running a session
  prints a clear error.

---

## Hard-won lessons

The distilled content of every release entry this file used to carry. Each item is here
because it cost time, shipped a defect, or made a check report the wrong answer — not
because it describes what changed.

### 1. Verification — the oracle that answers a different question

This is the single most repeated failure in this project's history, in enough different
shapes that it is worth naming them. **An oracle that can succeed for the wrong reason is
not a check.** Watch every guard fail on the defect it exists to catch, before believing it.

- **A guard read from `Cargo.lock` can never fail.** `cargo test` re-resolves and rewrites
  the lockfile before the test runs, so a deliberate downgrade was silently undone and the
  guard always observed a fixed version. Caught only by printing the lockfile before *and*
  after the run. The working guard is the **dependency floor in `Cargo.toml`** — cargo cannot
  resolve a vulnerable pair at all, which is stronger than a test that has to be reached
  *and* has to reproduce.
- **A behavioural repro that passes on the broken version discriminates nothing.** The
  stall-the-reader test for the quinn teardown passed on the vulnerable 0.11.15 too: the real
  failure needs GRO batching so frames are small slices of large allocations, which an
  in-process loopback test at modest rate never reaches.
- **A negative control must assert a clean baseline first.** `text_check.py`'s first controls
  cloned `HEAD`, which still carried the corruption being fixed, so three of four reported
  the *pre-existing* defect and "passed" without ever testing what they injected. A control
  that cannot distinguish its own mutation from existing damage is not a control.
- **A sabotage control must assert its match count.** A `str.replace` that matched zero times
  made the "sabotaged" run pass and look like evidence.
- **Strip comments before a structural check.** The check that `just clean` contains no
  `pkill` passed on the *comment explaining why it no longer does*.
  `scripts/gate_conformance.py` strips comments first for the same reason.
- **A test that re-implements the logic passes just as happily when the real code is wrong.**
  The `ETRCMD`/`ETRX11` tests rebuilt the parse loop inside the test. The bootstrap handling
  moved into `parse_bootstrap_line` and the bind default into `effective_bind_ip` precisely
  so tests call the shipped code.
- **A control can observe a dying process as alive.** `pkill` sends SIGTERM and `etrs` takes
  ~0.3 s to die, so the negative control reported the *dangerous* code safe. A four-second
  settle makes it fail correctly.
- **Under a recipe's `set -e`, a bare call aborts before its exit code can be read.** A test
  whose expected result is a non-zero exit is therefore unreachable, and fails with no
  message at all. Write `CMD || RC=$?`.
- **A harness that extracts part of a recipe must prove its extraction boundary before
  running anything.** The first `brew-publish` confirmation harness stripped indentation, so
  the closing `esac` never matched and the accepting cases ran on into the publish code. They
  stopped at a `git clone` of an empty URL, so nothing was pushed — by luck, not by design.
  The fixed harness asserts the extracted script contains no `clone` or `info` line.
- **`man-check` was looking at the wrong tree.** It rebuilt from the worktree and compared the
  worktree, while the tag tarball — which COPR and Homebrew install the pages from — carries
  the **commit**. It now renders from HEAD's own sources and version and compares HEAD's
  committed pages. HEAD is compared with *itself*, never with the worktree: comparing the two
  would fail every uncommitted bump mid-work, and **a guard that fires on correct work gets
  switched off, taking the real rule with it.**
- **`git diff --quiet` cannot see untracked files.** On a brand-new empty Homebrew tap it
  reports "no changes", so a publish exits 0 having pushed nothing — the worst available
  outcome. Both the tap and AUR pushes stage first and then ask `git diff --cached`.
- **Never build a line-ending check out of `grep`.** `grep -c $'\r'` from an agent shell is
  `~/AGENTS.md` §17: the pattern collapses to empty, `grep` matches every line, and the answer
  is the file's **line count** wearing a carriage-return costume. The tell is that it equals
  `wc -l`. `git ls-files --eol` (`i/lf w/crlf`) is the oracle that can actually answer.
- **A guard that fires on correct code is deleted within a week.** The first `@`-inside-a-
  shebang-recipe detector flagged `@` lines inside a `cat <<'MSG'` heredoc, which are data.
  Its self-test now pins all four outcomes, three of which are ways the check could be
  *wrong*.
- **Reasoning from a plausible mechanism is how this file got things wrong.** "UDP is limited
  by protobuf encoding" survived several releases because it came with a number attached; the
  number was the test harness's own `thread::sleep` and the encode path is ~3,600× faster
  than the figure it was blamed for. One benchmark would have refuted it. Likewise "`etrs`
  ignores SIGTERM" — the handler fired every time; the process could not exit. The 2×2 that
  settled it took twenty minutes and would have been cheaper than writing the speculation
  down.

### 2. Testing conventions

- **Every new guard is watched failing** via a negative control on the specific defect it
  exists to catch, and the controls live in each helper's `--self-test`, which `just check`
  runs.
- **e2e recipes snapshot matching pids before they start and reap only what appeared since**
  (`scripts/e2e_procs.sh`). Matching by *name* cannot tell a leftover from somebody's live
  session; a pid that did not exist a moment ago can only be ours. Running a test suite is
  not a request to drop the user's sessions — and this project's own use case is developing
  remotely on the machine being tested.
- **Never `pkill -f`.** A full-command-line match also matches the recipe's own shell, whose
  command line contains the pattern (`~/AGENTS.md` §10, three separate incidents). Use
  `pgrep -x`, a pid, or a pidfile. It has been hit *again* here, in a test harness, after
  being documented twice.
- **Some e2e parts cannot use tmux.** A tmux pane's stdin is a pty that never EOFs, so a
  tmux-hosted run cannot reach the stdin-EOF paths at all. Those parts need a controlling
  terminal *and* a redirected fd 0 simultaneously: a real `pty.fork` with `</dev/null` inside
  it.
- **Detach with `os.setsid()` from Python, not the `setsid(1)` binary** — it does not exist
  on macOS.
- **A structural check proves a guard is present, not that it works.** `gate_conformance.py`
  asserts the `pr`/`open-pr`/`merge-pr` guards exist because executing those recipes from
  `check` would be slow and occasionally destructive. It says so rather than implying more.

### 3. Transport and runtime

- **`stream_receive_window / frame_payload` must stay under quinn's assembler chunk cap
  (1024).** Advertising 4 MB to a library that can retain 1024 buffers was inconsistent by
  3.4×, which turned an upstream latent bug into a certainty under load: any sustained
  port-forward tore down the **entire QUIC connection**, interactive shell included, with
  `INTERNAL_ERROR: too many gaps in stream buffer`. 4 MB is restored only because
  quinn-proto 0.11.18 made the retained count self-limiting by arithmetic; the guard is the
  `quinn = "0.11.12"` floor in `Cargo.toml`. There is also a test asserting the *relationship*
  rather than the number.
- **"Gaps" did not mean packet loss.** Across every failing run the kernel's UDP
  `RcvbufErrors` and `SndbufErrors` moved by exactly **0**. A relay blocked in `write_all` on
  ordinary TCP back-pressure simply stops draining its QUIC stream, and quinn keeps one chunk
  per received STREAM frame. With no drops, a drop-related explanation could not be right —
  that measurement is what exposed the wrong reasoning.
- **A fix that moves the threshold is not a fix.** Enlarging the UDP socket buffers and
  shrinking `send_window` each delayed the failure (2.5 s → 30.7 s of survival), which is
  exactly why they were convincing. Neither *bounds* the chunk count. Both reverted.
- **Measure throughput one-way.** Every figure this project had came from an **echo**
  workload, so every byte crossed the link twice and the number was *offered load*, not
  goodput — not comparable to iperf3. That misreading was real: etr's echo figure was read as
  "29% of the path" when measured properly it reaches **97%** of raw single-stream TCP on a
  wifi WAN link and 184% on a wired one. The 184% is not etr beating the network: the baseline
  is one untuned TCP stream and QUIC's loss recovery genuinely beats that. A baseline is a
  floor, not a capacity. `just throughput-local` / `just throughput-remote HOST` always report
  both, from the same code, back to back.
- **Do not chase the loopback number.** etr shows ~3.9–4.1 Gb/s against raw TCP's ~28 Gb/s
  because quinn caps datagrams at 1452 while loopback MTU is 65536 — 6.2× more packet
  operations, each carrying AEAD, header protection, ACK tracking and congestion bookkeeping.
  Raising `upper_bound(65527)` measured **28% slower** and was reverted. It measures per-packet
  CPU overhead on a path nobody runs over.
- **A stale baseline produces a confident nonsense ratio.** The first corrino→gimli comparison
  read 207% because the baseline was minutes old and used a different binary. Re-run both
  sides back to back with identical binaries.
- **`-C target-cpu=native` in a user-level cargo config makes cross-host testing SIGILL** —
  and the helper survives *startup*, failing only once data flows, so a `--version` probe does
  not catch it. `throughput-remote` builds its own portable helpers. The repo has no
  `.cargo/config.toml`, so released binaries are unaffected.
- **A one-connection sink is destroyed by a readiness probe.** An `ncat -z` check consumed the
  sink's only connection; `tcp-sink` now keeps accepting and finishes only on a connection
  that carried data.
- **Dropping the PTY stream's sender finishes the client→server half, which the server reads
  as "session over".** Half-closing means keeping the sender alive, not merely not exiting.
  And a PTY cannot be half-closed at all, so stdin EOF is relayed **in band** as `VEOF`
  (`0x04`) — exactly what a terminal does on Ctrl-D. Gated on `has_remote_command`: an
  interactive shell never exits on its own, so holding the session open past stdin EOF would
  turn a prompt return into a hang.
- **Dropping the Tokio runtime waits for `spawn_blocking` tasks, and two of them cause the
  condition they wait on.** The PTY reader holds a clone of the master, so the master is never
  fully dropped, so the kernel never sends a hangup, so the command never exits, so the reader
  never unblocks. A self-sustaining deadlock — which is why "let the PTY close and hang the
  shell up" does not work and the child must be signalled: `SIGHUP` to its process group on
  every path out of the reconnect loop, escalating to `SIGKILL` after a second (verified: a
  command installing `SIG_IGN` for SIGHUP still hung past 25 s with SIGHUP alone). The
  deciding variable is **interactive vs remote-command**, not signal vs expiry — an
  interactive login shell exits on its own and breaks the deadlock, which is why the bug was
  invisible in nearly all use.
- **`std::process::exit()` is not available as a shortcut.** `X11Cleanup::drop` removes
  `/tmp/.X11-unix/X<n>` and two xauth entries; skipping destructors leaks both per
  X11-forwarded session.
- **`restore_terminal` must only run when raw mode was actually entered** (`RAW_EVER_ENABLED`
  enforces it inside the function). Otherwise 70 bytes of VT escapes land in a redirected
  stdout, corrupting exactly the command output the no-terminal path exists to deliver.
- **Record the utmp logout on *every* exit path.** The reconnect-expiry arm just `break`ed, so
  the single most likely way for a session to end — the user walking away — was the one that
  left a stale entry.
- **`peer.ip().to_canonical()` before writing utmp**, or IPv4 arrives as `::ffff:127.0.0.1`.
  And `dg.peer_port as u16` silently truncates — port 65536 becomes 0 — which sent datagrams
  somewhere plausible and wrong until `forward::datagram_peer_addr` started rejecting
  out-of-range ports.
- **`AddrPref::Auto` means "what this call site did before", not one global default.** The
  QUIC path took the resolver's first answer while `resolve_udp_target` has preferred IPv6
  since v0.4.x; a single "no preference = IPv6 first" would have silently changed the QUIC
  path for every unflagged user. `-4`/`-6` are a **preference**, not a restriction, unlike
  `ssh -4`/`-6` — nothing here can turn a working connection into a failure. Consequence:
  `etr` cannot just forward `-6` to `ssh` (ssh would hard-fail where etr would fall back), so
  it resolves first and passes the flag only when the host really has such an address.
  `TcpStream::connect("host:port")` walks candidates in the *resolver's* order, so forwarded
  TCP needed `forward::connect_tcp_preferred` or the flag would have been silently ignored
  there.
- **Windows console input:** `std::io::stdin().read()` goes through Rust std's `ReadConsoleW`
  shim, which **drops bytes that are not clean UTF-8** — that is what "ate" special characters.
  Read the console input handle directly with `ReadFile`, with `ENABLE_VIRTUAL_TERMINAL_INPUT`
  on and the input codepage set to UTF-8. Separately, a `ReadFile` *issued* while the console
  is still in cooked mode stays line-buffered for that whole read, so the reader must wait on
  a one-shot signal fired after raw+VT mode is enabled — a timing problem, not a
  read-mechanism one. And `disable_raw_mode` never clears the VT-input flag, so the original
  console input/output modes and codepage are captured once and restored verbatim, or the
  *local* shell stops accepting Enter after etr exits.
- **`build.rs` scans all `/usr/lib/*/` multiarch directories for `libutempter`.** Hardcoding
  `x86_64-linux-gnu` silently skipped linking on aarch64 and the release build failed at tag
  time, because CI's matrix did not mirror release's. It does now.

### 4. Release and packaging

Five channels: GitHub releases, crates.io, the AUR, COPR and a Homebrew tap. The order in
`AGENTS.md` §6 is not arbitrary — the AUR and Homebrew consume artefacts that only exist once
the GitHub release has been built.

- **No packaging file records a version or a checksum.** Every template carries
  `@VERSION@`/`@SHA…@` sentinels rendered at publish time by `scripts/render_packaging.py`.
  Five channels restating one fact is five chances to get it wrong; the sibling repo `retch`
  paid that bill in full — eleven releases of PKGBUILD drift with every CI run green, and a
  near-miss that came within one command of publishing an untagged version to crates.io. **A
  stale checksum cannot be committed here because no checksum is committed**, so the guards
  only have to assert the sentinels are still present.
- **`packaging/metadata.toml` is the single source of truth** for the summary, short summary,
  licence, GitHub About box and COPR page. Two summary lengths rather than one, and not for
  style: `brew audit` caps `desc` at 80 characters and rpmlint caps `Summary:` at 79, and brew
  also rejects a leading article, a trailing full stop and the formula's own name.
- **A new binary, man page or completion must reach FOUR places**, not three: the COPR spec's
  `%install`/`%files`, the Homebrew formula's `install`, the PKGBUILD's `package()`, **and**
  the `extras` job in `release.yml`. The AUR package is a `-bin` package with no source tree,
  so its man pages and completions come out of `etr-extras.tar.gz` — **a PKGBUILD cannot
  install a file the tarball does not carry**, the two files cannot see each other, and the
  mismatch builds fine in CI and fails on a user's machine.
- **Never run a downloaded binary in `package()`.** Generating completions that way breaks the
  moment the build host's architecture differs from the target's — not hypothetical for a
  package built for x86_64 *and* aarch64. Completions are architecture-independent (clap
  derives them from the CLI definition, no `cfg` inside either `Cli` struct) and CI *asserts*
  that: both Linux runners generate a set natively and the `extras` job hard-fails on a byte
  of difference.
- **Man pages are tracked** (`man/etr.1`, `man/etrs.1`). A GitHub tag tarball contains only
  tracked files, and both COPR and Homebrew `install` a page out of it; while they lived in a
  gitignored `man/build/` neither channel could ship one. The `.TH` line embeds the version, so
  *every* bump changes them — which is what makes `man-check` a gate rather than a wart.
- **`just publish` refuses unless `HEAD` is the tag for the version in `Cargo.toml`.**
  `cargo publish` uploads whatever the worktree says, and a crates.io version can be yanked but
  never deleted. A clean working tree is **not** the same check — the sibling's near-miss had a
  clean tree that simply named an unreleased version.
- **Check the publish prerequisites before tagging, not after.** Tagging starts `release.yml`
  and `copr.yml` on its own, so a missing crates.io token or AUR host key leaves the release
  half-published. That has already happened.
- **`git clean -fdx` would delete `WIP.md`.** It is gitignored *precisely because* it is the
  Syncthing-synced handoff file another section of `AGENTS.md` requires you to maintain, and
  `.claude/` goes the same way. Always preview with `--exclude=WIP.md --exclude=.claude`, and
  actually read the preview — that is the only reason this was caught.
- **GNU tar's `--exclude-vcs-ignores` does not implement `.gitignore` semantics.** Measured
  here: it would have packed **14,347** ignored entries — `target/`, `.claude/`, `WIP.md` —
  into the SRPM and on to the published `-debugsource` package. An SRPM cannot be recalled, so
  `.copr/Makefile`'s guard is a hard failure asserting both directions: files that must never
  ship, and files whose *absence* would be just as bad (no `Cargo.lock` means unpinned
  resolution; no `LICENSE` means a GPL obligation shipped unmet).
- **rpm compresses man pages**, so `%files` must glob `etr.1*`. Asserting the uncompressed name
  reports a present file as missing.
- **`std_cargo_args` already passes `--locked`**; writing the flag out as well makes cargo
  refuse the build outright.
- **A sentinel inside a comment is still substituted** — the first render produced a spec whose
  header comment read "THIS IS A TEMPLATE. `0.8.3` is filled in by…". Comments now describe the
  sentinels instead of containing them.
- **Filter the artefacts the release job attaches.** With `merge-multiple: true` and no
  `pattern`, the raw per-architecture completions would have been published as six loose files
  on the release page — the exact outcome packing them into one asset was meant to avoid. The
  job downloads `pattern: release-*`, which makes the artifact naming convention load-bearing.
- **A `%changelog` weekday that disagrees with its date builds and ships anyway** (COPR logged
  `bogus date` and published). The generator was never at fault: it derives the weekday with
  `strftime`, but it is deliberately idempotent, so it stepped over a hand-written seed entry
  and *preserved* the wrong date. A safety property protected a defect. `check_changelog_dates`
  now closes the class. Spell the month table out — `strptime("%b")` follows `LC_TIME` and
  misparses on a non-English host.
- **The AUR serves stale reads for minutes after a push, and cgit is not uniformly fresh.**
  Verify with a fresh clone (`~/AGENTS.md` §6), never a single HTTP endpoint. The push output
  naming a commit range is itself proof the server-side hook accepted it.
- **Verify each channel after a release rather than inferring it from the push output.**

### 5. Repo and tooling conventions

- **Cross-repo staleness is this project's most repeated defect.** `etr`, `retch` and
  `rusticprofile` share a Portable Core and a vendored Justfile block, and fixes have
  repeatedly failed to cross: the CI merge gate, `.gitattributes`, `PR_CONFIRM`,
  `just open-pr` and `install-hooks` all existed in a sibling for months before reaching here,
  and the three attribution sub-bullets reached this repo only after **five commits on `main`
  had already shipped duplicate trailers**, one of them with four. When a rule lands in Part 1
  of `AGENTS.md`, propagate it in the same round; when evidence is repo-specific, it belongs
  here, not in Part 1.
- **The block marker version and each helper's `TEMPLATE_VERSION` move independently**, because
  a helper can change without the block changing. Three repos once all declared template v3
  while the bodies differed — each agreed with *itself*, so nothing looked wrong from inside any
  of them, which is exactly the failure a version marker exists to prevent.
- **`@` at the start of a line in a `#!` recipe is not just's line-suppression prefix.** just
  strips it only in *plain* recipes; a shebang recipe gets a command literally named
  `@/usr/bin/python3` and exits 127 — *after* the work has happened, so the recipe looks broken
  when only its last line was. `gate_conformance.py` refuses it in any recipe, skipping
  heredocs.
- **`Path("/dest") / "/abs/path"` discards the left operand.** In `install_completions.py` an
  absolute argument relocated every write onto the path itself, which under `--from-path` is the
  installed binary — a 3.6 MB executable replaced by a 21 KB completion script, exit 0, nothing
  printed. The flag is *called* `--from-path`, which invites precisely the argument that breaks
  it, so documenting it would not have helped. `reject_path_like()` makes it unexpressible.
- **A confirmation prompt must say what it accepts.** `BREW_CONFIRM` required the literal `yes`
  while `PR_CONFIRM` and `CLEAN_CONFIRM` took `y`, and it fired mid-release — aborting at the
  Homebrew leg *after* crates.io and the AUR had published. All three now accept
  `y`/`Y`/`yes`/`YES` and every path prints `[y/N]`. **Widening who can answer is not widening
  what counts as an answer**: an empty answer, a stray newline, `Yes` and an unset variable all
  still refuse.
- **A gate must be answerable without a terminal.** A bare `read` blocks a script, CI job or
  agent on a stdin nobody holds, or dies without saying why — which reads as the gate *refusing
  the change* rather than asking a question nobody could hear.
- **LF is the base model for every non-binary file.** This fleet is Linux-primary and the tree
  is Syncthing-shared across three OSes, so `.gitattributes` pins `* text=auto eol=lf`;
  without it a Windows checkout writes CRLF, Syncthing propagates it, and git elsewhere
  reports a phantom whole-tree diff (retch measured 13,811 insertions / 13,811 deletions, all
  line-ending flips). Surveyed by byte count 2026-09-22: **71 tracked text files, 0 carriage
  returns**, and `WIP.md` 0 — etr was already clean, while retch's `WIP.md` was the single
  CRLF file across all three repos and was converted in its v0.18.1.
- **`WIP.md` is the one text file nothing reaches through git.** It is gitignored, so
  `.gitattributes` never applies and `text_check.py` — which walks `git ls-files` — cannot see
  it, while `scripts/reset_wip.py` rewrites it on every `just merge-pr`. `just wip-check`
  (in `just check`) supplies the missing guarantee.
  - **The rewrite preserves the terminator it finds rather than hardcoding LF**, and that is
    deliberate: `read_text()`/`write_text()` translate silently, so hardcoding converts a file
    as a *side effect of a merge* — which is the accident retch shipped in its v0.17.12,
    pointing the other way. Reading and writing bytes keeps the conversion out of the merge
    path; `--check-endings` is where the decision is stated, somewhere a reader can see it.
  - **A rewrite that reports success without finding its target is the same family as every
    other entry in §1.** Both substitutions used to be unbounded, so while `WIP.md` was a
    rolling log the `### Active Branch:` pattern rewrote every historical heading — and the
    `**Latest commit on main**` pattern matched **nothing at all**, while the script still
    printed "Latest commit updated to …". Exactly one match of each is now required, and the
    script writes nothing on any other count.
  - **Anchor a marker pattern to line start.** The exactly-one rule fired on its first real
    run — because `WIP.md`'s own header *names* both markers in prose, and an unanchored
    pattern matched those mentions too. A guard whose first firing is a false positive is one
    commit away from being loosened instead of fixed; anchoring is what the pattern actually
    meant, and the self-test now pins that a prose mention survives while the heading is
    rewritten.
- **A double backslash in an agent shell command arrives as a single one** (`~/AGENTS.md` §14),
  before the shell sees it, so a quoted heredoc does not prevent it. Two collapsed backslashes
  sat in this file for eight releases — one of them turning into a real newline and **splitting
  a paragraph in half**, which is why review never caught it: the file still rendered, just
  wrongly. Write `chr(92)` in Python; never a backslash literal in a shell string.
- **Claude Code Review runs on `workflow_dispatch` only.** The token behind it has failed in a
  way worse than useless — in a sibling repo, 19 green runs then failing *every* run in ~490 ms
  at $0.00 with no findings. A rejection before any tokens are billed is a credential or quota
  problem, not a verdict on the code, but left on `pull_request` it becomes a red check on every
  future PR and trains everyone to merge over failing checks. The trigger is kept commented
  immediately below, so restoring it is uncommenting two lines. Deliberately **not** copied from
  retch: its version also sets `if: false`, so a manual dispatch appears to run and silently does
  nothing.
---

## Test coverage (153 tests)

| Module | What's tested |
|--------|--------------|
| `quic` | Transport bounds: the window/chunk-cap relationship is asserted as documentation, with the real guard being the `quinn` floor in Cargo.toml (see v0.9.4 — two test-shaped guards were tried and neither could discriminate). Cert generation, server/client config, write/read Envelope framing, write/read PTY chunk framing |
| `protocol` | SessionOpen/Accept encode-decode (incl. `gateway_ports`, `reverse_forwards`, and `x11_enabled`/`x11_auth_proto`/`x11_auth_cookie` round-trip), StreamOpen/Close, Heartbeat, Disconnect, UdpDatagram |
| `session/stream` | Acknowledge edge cases, replay from 0, initial seq values |
| `session/mod` | Close/ack unknown stream, `last_received_map` semantics, collect_replays, `open_stream` idempotence |
| `bin/etrs` | CLI defaults, verbose count, custom port, subcommand parsing, hex_decode, custom --log-path override, `ETRCMD`/`ETRX11`/`ETRPREFER` bootstrap line parsing (through the shipped `parse_bootstrap_line`), `-4`/`-6` flags and their mutual exclusion, `effective_bind_ip` defaults vs explicit `-b`, client preference overriding the server's own |
| `login` | no-panic checks for record_login / record_logout with invalid fd |
| `bin/etr` | CLI defaults, port parsing, target parsing, `-4`/`-6` short and long forms, their mutual exclusion, a family flag ahead of a remote command, CLI-beats-config precedence, no --cipher flag, custom --log-path and --server-log-path overrides, config fallback for log paths, terminal-restore sequences (cursor-safe modes cover mouse/paste/cursor and never move the cursor; screen reset leaves alt-screen without clearing scrollback) |
| `config` | TOML parse (full section, partial, empty), default values, `gateway_ports` / `forward` / `reverse_forward` / `x11` / `x11_trusted` / `address_family` config keys, `merge_defaults` idempotence |
| `forward` | UDP fast path: `UdpFrameEncoder` byte-identical to the pre-0.9.2 inline encoder (IPv4/IPv6, empty and 65507-byte payloads), alternating peers, port-only change, round trip through the decoder; `datagram_peer_addr` accepts both families and rejects port 0, ports above 65535 and non-literals. `-L`/`-R` spec parsing: TCP/UDP/IPv6, explicit proto, bad port, empty host, Display; bind address parsing (explicit IP, `[::1]`, wildcard `*`); `get_bind_addresses` with and without gateway flag; `resolve_udp_target`: localhost prefers IPv6, explicit IPv4, unresolvable host, `-4` overriding the IPv6-first default, fallback to the other family; `connect_tcp_preferred`: reaches an IPv4 listener, falls back across families, errors on an unresolvable host; `X11Display` parsing |
| `addrfam` | `AddrPref` from flags / config aliases / `ETRPREFER:` wire values (unknown → `Auto` in every direction), `or` fallback, ssh flag mapping, `order_by_family` (preferred family first, stable within a family, identity for `Auto`, single-family and empty input), `first_routable`, `resolve_preferred`, `family_available` |
