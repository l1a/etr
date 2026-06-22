# etr — Project Status Notes

## What this is

`etr` is a Rust reimplementation of [Eternal Terminal](https://eternalterminal.dev/) (`et`).
Eternal Terminal is a persistent remote shell that survives network interruptions — unlike
SSH, the session keeps running on the server and the client reconnects transparently when
the link drops.  This project uses **QUIC** (via the `quinn` crate) for the transport
layer, which provides reliable, ordered, multiplexed streams with congestion control
and TLS 1.3 built-in.

## Current state: v0.4.22 — remote command support

New in v0.4.22:
- `etr host [command [args...]]`: optional trailing arguments run a remote
  command under the PTY instead of an interactive shell.
  Multiple words are joined with spaces and passed to `sh -c`, so shell
  metacharacters (pipes, redirects) work and full-screen TUI programs like
  `btop` and `distrobox` work correctly.  The session ends when the command
  exits.  Example: `etr host 'distrobox -- btop'`.
- Bootstrap protocol: client writes `ETRCMD:<command>` as an extra line after
  env vars; old servers ignore it (no `=` → silently skipped).
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
# Debug build (fast, for development)
cargo build
just install          # copies target/debug/{etr,etrs} to ~/.cargo/bin

# Release build
cargo build --release
just install-release  # copies target/release/{etr,etrs} to ~/.cargo/bin

# Code quality gate — run before every commit
just check            # cargo fmt --check + cargo clippy -D warnings
just test             # cargo test (103 tests)
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
- **PQC key exchange**: ML-KEM was retired with the QUIC migration.  Can be re-added
  via `rustls-post-quantum` (X25519MLKEM768 hybrid) once it stabilises.
- **macOS**: fully tested and working.  PTY session, reconnect, and port forwarding
  all pass.  Test harness fixes applied: `ps -o ppid=` replaces Linux-only
  `/proc/$$/status`; reconnect test stops the etrs daemon (not the etr client)
  because stopping a PTY-attached process on macOS triggers a SIGHUP that kills it.
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

## Test coverage (103 tests)

| Module | What's tested |
|--------|--------------|
| `quic` | Cert generation, server/client config, write/read Envelope framing, write/read PTY chunk framing |
| `protocol` | SessionOpen/Accept encode-decode (incl. `gateway_ports` and `reverse_forwards` round-trip), StreamOpen/Close, Heartbeat, Disconnect, UdpDatagram |
| `session/stream` | Acknowledge edge cases, replay from 0, initial seq values |
| `session/mod` | Close/ack unknown stream, `last_received_map` semantics, collect_replays, `open_stream` idempotence |
| `bin/etrs` | CLI defaults, verbose count, custom port, subcommand parsing, hex_decode, custom --log-path override |
| `login` | no-panic checks for record_login / record_logout with invalid fd |
| `bin/etr` | CLI defaults, port parsing, target parsing, no --cipher flag, custom --log-path and --server-log-path overrides, config fallback for log paths |
| `config` | TOML parse (full section, partial, empty), default values, `gateway_ports` / `forward` / `reverse_forward` config keys |
| `forward` | `-L`/`-R` spec parsing: TCP/UDP/IPv6, explicit proto, bad port, empty host, Display; bind address parsing (explicit IP, `[::1]`, wildcard `*`); `get_bind_addresses` with and without gateway flag; `resolve_udp_target`: localhost prefers IPv6, explicit IPv4, unresolvable host |
