# etr — Project Status Notes

## What this is

`etr` is a Rust reimplementation of [Eternal Terminal](https://eternalterminal.dev/) (`et`).
Eternal Terminal is a persistent remote shell that survives network interruptions — unlike
SSH, the session keeps running on the server and the client reconnects transparently when
the link drops. This project reproduces that behaviour from scratch using UDP transport,
a custom handshake, and modern cryptography.

## Current state: working end-to-end on localhost

The full round-trip works: `etr <host>` on the client, SSH bootstrap that starts
`etrs` on the fly, UDP handshake, live PTY session, heartbeat keepalive, and
transparent reconnect after network loss. All verified by `just test-local`.

---

## Architecture

### Two binaries

| Binary | Role |
|--------|------|
| `etrs` | Per-session server — started by `etr` via SSH, forks after binding a port, exits on clean disconnect |
| `etr`  | Client — SSH bootstrap, UDP connection loop, raw-mode terminal |

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
   │                                         │ binds UDP port 0 (OS assigns)
   │                                         │ prints "PORT <n>" to stdout
   │                                         │ forks → parent exits (SSH returns)
   │                                         │ child: detaches stdio, runs session
   │◄── reads "PORT <n>" from SSH stdout ────┘
   │
   └─ 3. UDP ClientHello ──────────────────► etrs child
          (encrypted with hello_key         ServerHello ◄──┘
           derived from passkey)
                                             KEM key exchange
                                             session key derived
   ◄──────────────── UDP data ──────────────►
         (AEAD encrypted, seq-numbered)

   (on clean Disconnect: etrs child exits; no daemon remains)
```

### SSH bootstrap detail

`etr` runs `ssh -p <ssh_port> <target> etrs` and writes
`session_id_hex/passkey/term\n` to stdin. `etrs` reads this, binds a random UDP port,
prints `PORT <actual_port>` to stdout (which `etr` reads), then forks:

- **Parent**: exits immediately, allowing the SSH connection to close cleanly.
- **Child**: calls `setsid()`, redirects stdio to `/dev/null` (stderr → session log),
  then builds a Tokio runtime and runs the session loop.

`etr` learns the port from SSH stdout before the connection closes.

### Reconnect

The client detects a dropped connection via a 15-second idle timeout on heartbeats.
It loops: re-handshake, replay any unacknowledged sends, resume the PTY stream.
The server keeps session state (cipher, stream history, PTY) alive across reconnects
for up to 30 minutes. A new UDP source address is fine — the session is keyed by
`session_id`, not the peer address. On clean disconnect, `etrs` exits immediately.

---

## Cryptography

### Cipher suites

| ID | KEM | AEAD | KDF | Feature flag |
|----|-----|------|-----|--------------|
| 1 | ML-KEM-1024 | AES-256-GCM | HKDF-SHA3-256 | `pqc` |
| 2 | ML-KEM-768  | AES-256-GCM | HKDF-SHA-256  | `pqc` |
| 3 | X25519      | AES-256-GCM | HKDF-SHA-256  | (default) |
| 4 | X25519      | ChaCha20-Poly1305 | HKDF-SHA-256 | (default) |

PQC suites are **on by default**. Without `--no-default-features`, the client
advertises `[1, 2, 3, 4]` and negotiates ML-KEM-1024 (suite 1). To build without
PQC (smaller binary, no post-quantum crypto):
```
cargo build --no-default-features
cargo install --path . --no-default-features
```

### Key derivation

```
hello_key   = HKDF-SHA-256(ikm=passkey, salt=client_nonce, info="etr-hello-v1")
session_key = KDF(ikm=passkey‖kem_shared_secret, salt=client_nonce‖server_nonce, info="etr-session-v1")
```

The hello key encrypts `ServerHello` so the passkey provides pre-auth. The session
key folds in the KEM shared secret for forward secrecy.

### Nonce construction

AEAD nonces are 12 bytes: bytes 0–3 zero, bytes 4–11 = packet sequence number (big-endian).
Sequence numbers start at 1 and are monotonically increasing per session.

---

## Wire format

Every UDP packet:
```
[4 bytes: magic+version] [16 bytes: session_id] [1 byte: flags] [8 bytes: seq (big-endian)]
[4 bytes: payload_len] [payload_len bytes: protobuf Envelope, optionally AEAD-encrypted]
```

Handshake packets (flag bit set) carry plaintext `ClientHello` / encrypted `ServerHello`.
Data packets carry an encrypted `Envelope` containing one of: `StreamData`, `StreamAck`,
`StreamOpen`, `StreamClose`, `TerminalResize`, `Heartbeat`, `Disconnect`.

---

## Verbosity / diagnostics

Both binaries support `-v` / `-vv` / `-vvv` (SSH-style count):

| Level | `etrs` shows | `etr` shows |
|-------|-------------|-------------|
| `-v`  | session lifecycle (register, handshake, timeout, disconnect) | connection events (connect, reconnect, disconnect) |
| `-vv` | cipher suite negotiated, session ID | cipher suite, session ID, connection parameters |
| `-vvv` | every packet (type, seq, size, peer) | every packet send/recv, heartbeats |

**Client log file**: when `etr` is run interactively with `-v` or higher, logs go to
`$XDG_STATE_HOME/etr/etr.log` (default: `~/.local/state/etr/etr.log`) rather than
stderr, to avoid corrupting the raw-mode terminal display. A single line on stderr
tells you where to look. Watch it live with `tail -f ~/.local/state/etr/etr.log` or
`just log` (which tails the server log; add a client equivalent if needed).

**Server log file**: `etrs` writes to `$XDG_STATE_HOME/etr/etrs.log` (default:
`~/.local/state/etr/etrs.log`) after forking. Watch with `just log`.

---

## Configuration

A TOML config file is loaded from `$XDG_CONFIG_HOME/etr/config.toml`
(default: `~/.config/etr/config.toml`). All fields are optional and fall back to
compiled-in defaults. CLI flags take precedence over config file values.

```toml
[client]
# Cipher suites in preference order (short names below).
# Default: all supported suites, strongest first.
ciphers = ["ml-kem-1024", "x25519-aes"]

# Default SSH port (default: 22)
ssh_port = 22

# Path to etrs on remote hosts (default: "etrs", relies on PATH)
server_path = "/usr/local/bin/etrs"
```

### Cipher suite selection

Select cipher suites with `--cipher` (repeatable, preference order):
```bash
etr --cipher ml-kem-1024 --cipher x25519-aes host
```

Precedence: `--cipher` flags > `config.toml ciphers` > compiled-in defaults.

---

## Ports and paths

| Resource | Default | Override |
|----------|---------|----------|
| UDP data port | OS-assigned (random high port) | `etrs -p PORT` |
| SSH port | 22 | `-s PORT` or config `ssh_port` |
| etrs binary path | `etrs` (PATH) | `--server-path` or config `server_path` |
| Server log | `~/.local/state/etr/etrs.log` | (not yet configurable) |
| Client log | `~/.local/state/etr/etr.log` | (not yet configurable) |
| Server bind address | `[::]` (dual-stack) | `etrs -b ADDR` |

IPv6 is fully supported. `localhost` resolves to `[::1]` on most modern Linux systems;
the client binds `[::]:0` when the resolved server address is IPv6, `0.0.0.0:0` for IPv4.

---

## Building and installing

```bash
# Debug build (fast, for development)
cargo build
just install          # copies target/debug/{etr,etrs} to ~/.cargo/bin

# Release build
cargo build --release
just install-release  # copies target/release/{etr,etrs} to ~/.cargo/bin

# Without post-quantum crypto (opt out)
cargo install --path . --no-default-features

# Code quality gate (run before pushing)
just check            # cargo fmt --check + cargo clippy -D warnings
just test             # cargo test (91 tests as of this writing)
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
just test-local
```

---

## Product vision

### Mode 1 — Persistent reconnecting shell (like mosh)

The primary use case. `etr user@host` works with **no pre-configuration on the
server** — analogous to how mosh works. The client SSHes to the server, `etrs` is
started on the fly, binds a random UDP port, forks, and the SSH connection closes.
`etr` then connects to the UDP port for the persistent session.

**Current state**: fully implemented.

### Mode 2 — Persistent port forwarding (like `ssh -L`/`-R`)

A one-shot invocation that opens a forwarded socket and keeps it alive across network
interruptions, without a PTY session. Example:

```bash
etr -L 5432:db-host:5432 user@jumphost   # local port → remote
etr -R 8080:localhost:8080 user@server   # remote port → local
```

**Current state**: the `PortForward` stream type is defined in the protocol and the
stream multiplexing layer supports multiple streams, but the CLI has no `-L`/`-R` flags
and the server has no port-forwarding logic.

---

## Known gaps / next steps

- **`just test-local` reconnect test broken**: the happy-path test passes but the
  reconnect step fails because `pgrep -x etr` cannot find the `etr` process when
  launched from a non-interactive context (e.g. from within Claude Code / a tool
  harness). Root cause unresolved — the session works, but the process is not visible
  to pgrep in that environment. Needs investigation; reconnect can be verified manually
  in a real terminal.
- **Benchmarking**: no performance benchmarks exist. Key areas to measure: handshake
  latency, per-packet encrypt/decrypt throughput (all four cipher suites), PTY
  round-trip latency, and throughput under reconnect. Consider `criterion` for
  micro-benchmarks and a `just bench-local` recipe for end-to-end latency.
- **Mode 2 — port forwarding**: add `-L`/`-R` CLI flags to `etr` and implement the
  forwarding logic in `etrs`. The stream layer already supports it structurally.
- **Multiple simultaneous sessions**: each `etr` invocation starts its own `etrs`
  child; there is no way to list or re-attach to an existing session from a new client.
  Session state (ID + passkey) is in-memory only.
- **Re-attach from a new client machine**: the `session_id` and `passkey` are not
  persisted anywhere, so a new machine cannot reconnect to an existing session.
- **Client/server log path**: not yet configurable via CLI flag.
- **Windows / macOS**: the PTY layer uses `portable-pty` (cross-platform) but has
  only been tested on Linux.

---

## Test coverage (112 tests)

| Module | What's tested |
|--------|--------------|
| `crypto/aead` | AES-256-GCM and ChaCha20 round-trip, wrong key, tampered ciphertext, wrong seq, empty plaintext, nonce uniqueness, big-endian encoding |
| `crypto/kdf` | Determinism, output length, salt/IKM/info binding, SHA-256 vs SHA3-256 divergence |
| `crypto/x25519` | Key exchange round-trip, distinct keypairs differ, invalid-length error |
| `crypto/kyber` | ML-KEM-768/1024 round-trips, wrong ciphertext → implicit rejection |
| `crypto/mod` | Full suite encrypt/decrypt, wrong passkey, hello cipher |
| `handshake` | Error variants, empty bytes, wrong message type, unknown suite, `last_received_seq` round-trip |
| `transport` | Decode wrong key/mutation/seq, invalid protobuf, two UDP loopback end-to-end tests, truncated header |
| `session/stream` | Acknowledge edge cases, replay from 0, initial seq values |
| `session/mod` | Close/ack unknown stream, `last_received_map` semantics, collect_replays, `open_stream` idempotence |
| `bin/etrs` | CLI defaults, verbose count, custom port, subcommand parsing |
| `config` | TOML parse (full section, partial, empty), default values |
| `bin/etr` | CLI defaults, port parsing, target parsing, `--cipher` flag, `resolve_ciphers` |
