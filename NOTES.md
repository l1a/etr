# etr — Project Status Notes

## What this is

`etr` is a Rust reimplementation of [Eternal Terminal](https://eternalterminal.dev/) (`et`).
Eternal Terminal is a persistent remote shell that survives network interruptions — unlike
SSH, the session keeps running on the server and the client reconnects transparently when
the link drops.  This project uses **QUIC** (via the `quinn` crate) for the transport
layer, which provides reliable, ordered, multiplexed streams with congestion control
and TLS 1.3 built-in.

## Current state: v0.4.0 — remote port forwarding support

The full round-trip works: `etr <host>` on the client, SSH bootstrap that starts
`etrs` on the fly, QUIC connection with cert pinning, PTY session, keepalives,
reconnecting after drops, `-L` local port forwarding, and `-R` remote port forwarding (both TCP and UDP).
Published to crates.io; `cargo install etr` installs both binaries.

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
just test             # cargo test (67 tests)
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
- **UDP reply routing**: current shared-socket design uses last-sender routing —
  replies from the remote UDP target go to whichever local client sent the most recent
  datagram.  Suitable for single-sender and sequential request/response (DNS, STUN);
  not suitable for multiple concurrent UDP senders to the same forwarded port.
- **Multiple simultaneous sessions**: each `etr` invocation starts its own `etrs`
  child; there is no way to list or re-attach to an existing session from a new client.
  Session state (ID + passkey) is in-memory only.
- **Re-attach from a new client machine**: the `session_id` and `passkey` are not
  persisted anywhere, so a new machine cannot reconnect to an existing session.
- **PQC key exchange**: ML-KEM was retired with the QUIC migration.  Can be re-added
  via `rustls-post-quantum` (X25519MLKEM768 hybrid) once it stabilises.
- **Windows / macOS**: the PTY layer uses `portable-pty` (cross-platform) but has
  only been tested on Linux.

---

## Test coverage (67 tests)

| Module | What's tested |
|--------|--------------|
| `quic` | Cert generation, server/client config, write/read Envelope framing, write/read PTY chunk framing |
| `protocol` | SessionOpen/Accept encode-decode (incl. `gateway_ports` and `reverse_forwards` round-trip), StreamOpen/Close, Heartbeat, Disconnect, UdpDatagram |
| `session/stream` | Acknowledge edge cases, replay from 0, initial seq values |
| `session/mod` | Close/ack unknown stream, `last_received_map` semantics, collect_replays, `open_stream` idempotence |
| `bin/etrs` | CLI defaults, verbose count, custom port, subcommand parsing, hex_decode, custom --log-path override |
| `bin/etr` | CLI defaults, port parsing, target parsing, no --cipher flag, custom --log-path and --server-log-path overrides, config fallback for log paths |
| `config` | TOML parse (full section, partial, empty), default values, `gateway_ports` / `forward` / `reverse_forward` config keys |
| `forward` | `-L`/`-R` spec parsing: TCP/UDP/IPv6, explicit proto, bad port, empty host, Display; bind address parsing (explicit IP, `[::1]`, wildcard `*`); `get_bind_addresses` with and without gateway flag |
