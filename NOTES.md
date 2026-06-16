# etr — Project Status Notes

## What this is

`etr` is a Rust reimplementation of [Eternal Terminal](https://eternalterminal.dev/) (`et`).
Eternal Terminal is a persistent remote shell that survives network interruptions — unlike
SSH, the session keeps running on the server and the client reconnects transparently when
the link drops. This project reproduces that behaviour from scratch using UDP transport,
a custom handshake, and modern cryptography.

## Current state: working end-to-end on localhost

The full round-trip works: `etrs daemon` on the server, `etr <host>` on the client,
SSH bootstrap, UDP handshake, live PTY session, heartbeat keepalive, and transparent
reconnect after network loss. All verified by `just test-local`.

---

## Architecture

### Two binaries

| Binary | Role |
|--------|------|
| `etrs` | Server daemon — listens on UDP, manages sessions, spawns PTYs |
| `etr`  | Client — SSH bootstrap, UDP connection loop, raw-mode terminal |

### Connection lifecycle

```
[client machine]                        [server machine]
  etr
   │
   ├─ 1. generate session_id + passkey (random)
   │
   ├─ 2. ssh target "etrs register"
   │       stdin: session_id_hex/passkey/term/reg_port
   │                                         │
   │                                   etrs register
   │                                         │ TCP 127.0.0.1:2023
   │                                         ▼
   │                                   etrs daemon
   │                                   (stores session)
   │
   └─ 3. UDP ClientHello ──────────────────► etrs daemon
          (encrypted with hello_key         ServerHello ◄──┘
           derived from passkey)
                                             KEM key exchange
                                             session key derived
   ◄──────────────── UDP data ──────────────►
         (AEAD encrypted, seq-numbered)
```

### SSH bootstrap detail

`etr` runs `ssh -p <ssh_port> <target> etrs register` and writes
`session_id_hex/passkey/term/reg_port\n` to stdin. The `etrs register` subcommand
reads this from stdin and forwards it to the daemon over a local TCP connection on
`127.0.0.1:(udp_port+1)` (default: 2023). This gives the daemon the passkey it needs
to decrypt the first `ClientHello` before the UDP client arrives.

**Why TCP not Unix socket**: A Unix domain socket path depends on `$XDG_RUNTIME_DIR`,
which is set in interactive sessions (by PAM/systemd-logind) but may not be set in
non-interactive SSH sessions. A TCP loopback port has no such environment dependency.

**Why `reg_port = udp_port + 1`**: No CLI flags needed on `etrs register` (clap
global-arg propagation to subcommands is unreliable after the subcommand name); the
port is passed through the existing stdin channel instead, and the daemon derives it
from its own UDP port so both sides always agree.

### Reconnect

The client detects a dropped connection via a 15-second idle timeout on heartbeats.
It loops: re-handshake, replay any unacknowledged sends, resume the PTY stream.
The server keeps session state (cipher, stream history, PTY) alive across reconnects
indefinitely. A new UDP port/address is fine — the session is keyed by `session_id`,
not the peer address.

---

## Cryptography

### Cipher suites

| ID | KEM | AEAD | KDF | Feature flag |
|----|-----|------|-----|--------------|
| 1 | ML-KEM-1024 | AES-256-GCM | HKDF-SHA3-256 | `pqc` |
| 2 | ML-KEM-768  | AES-256-GCM | HKDF-SHA-256  | `pqc` |
| 3 | X25519      | AES-256-GCM | HKDF-SHA-256  | (default) |
| 4 | X25519      | ChaCha20-Poly1305 | HKDF-SHA-256 | (default) |

Without `--features pqc`, suites 1 and 2 are compiled out and the client advertises
`[3, 4]`. Suite 3 (X25519+AES-256-GCM) is selected in normal use. To enable PQC:
```
cargo build --features pqc
cargo install --path . --features pqc
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
| `-v`  | session lifecycle (register, handshake, timeout, disconnect) | connection events |
| `-vv` | cipher suite negotiated, session ID | cipher suite, session ID |
| `-vvv` | every packet (type, seq, size, peer) | every packet send/recv |

**Client log file**: when `etr` is run interactively with `-v` or higher, logs go to
`$XDG_STATE_HOME/etr/etr.log` (default: `~/.local/state/etr/etr.log`) rather than
stderr, to avoid corrupting the raw-mode terminal display. A single line on stderr
tells you where to look. Watch it live with `tail -f ~/.local/state/etr/etr.log` or
`just log` (which tails the server log; add a client equivalent if needed).

**Server log file**: `etrs daemon` writes to stderr; redirect it yourself or use the
`just test-local` recipe which captures it to `$XDG_STATE_HOME/etr/etrs.log`.

---

## Ports and paths

| Resource | Default | Override |
|----------|---------|----------|
| UDP data port | 2022 | `etrs -p PORT` / `etr -p PORT` |
| TCP registration port | udp+1 = 2023 | (derived, not configurable separately) |
| Server log | stderr | redirect manually |
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

# With post-quantum crypto
cargo install --path . --features pqc

# Code quality gate (run before pushing)
just check            # cargo fmt --check + cargo clippy -D warnings
just test             # cargo test (91 tests as of this writing)
```

---

## Running

```bash
# On the server (or localhost for testing)
etrs daemon               # listens UDP [::]:2022, TCP reg 127.0.0.1:2023
etrs daemon -vvv          # with full packet trace

# On the client
etr user@host             # standard connect
etr localhost             # localhost testing (SSH to localhost must be configured)
etr -vvv host             # verbose — logs to ~/.local/state/etr/etr.log

# Prerequisites for localhost testing
ssh-copy-id localhost     # or append ~/.ssh/id_*.pub to ~/.ssh/authorized_keys
just check-tools          # verifies tmux, ssh, passwordless localhost SSH

# Full automated end-to-end test (happy path + reconnect)
just test-local
```

---

## Known gaps / next steps

- **`--server-path`**: `etr --server-path /full/path/to/etrs host` is available if
  `etrs` is not in the SSH session PATH, but with `~/.cargo/bin` in PATH it is
  not normally needed.
- **Port forwarding**: the stream multiplexing layer supports `PortForward` stream
  type but `etr` has no `-L`/`-R` CLI flags yet.
- **Multiple simultaneous sessions**: the daemon supports them (keyed by `session_id`)
  but there is no way to list or attach to existing sessions from the CLI.
- **Client log path**: not yet configurable via CLI flag.
- **Windows / macOS**: the PTY layer uses `portable-pty` (cross-platform) but has
  only been tested on Linux. The TCP registration approach removed the last
  Linux-specific path (`$XDG_RUNTIME_DIR`).
- **PQC**: ML-KEM-768/1024 is implemented and tested but not compiled in by default
  because `ml-kem` is a newer dependency. Enable with `--features pqc`.
- **Re-attach from a new client machine**: not yet implemented. Session state lives
  in the daemon process; a new machine would need the original `session_id` and
  `passkey`, which are not persisted anywhere.

---

## Test coverage (91 tests)

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
| `bin/etr` | CLI defaults, port parsing, target parsing (IPv6 brackets, user@host, host:port) |
