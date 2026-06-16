# etr

A Rust reimplementation of [Eternal Terminal](https://eternalterminal.dev/) (`et`) — a remote shell that automatically reconnects without interrupting your session.

Unlike SSH, when your network drops, `etr` keeps the remote shell alive and transparently reconnects when connectivity returns. Like mosh, no server daemon needs to be pre-installed or running: `etr` bootstraps a per-session server process via SSH, then hands off to a persistent UDP connection.

## Quick start

```bash
# Install
cargo install etr

# Connect
etr user@host

# Connect on a non-standard SSH port
etr -s 2222 user@host
```

The only requirement on the server is that `etrs` is in PATH (installed alongside `etr` by `cargo install`).

## How it works

1. `etr` SSHes to the server and starts `etrs`, which binds a random UDP port and forks into the background. The SSH connection then closes.
2. `etr` connects to that UDP port and runs a 1-RTT encrypted handshake (post-quantum by default: ML-KEM-1024 + AES-256-GCM).
3. All terminal I/O flows over the UDP session. If the connection drops, `etr` waits silently and reconnects when the network returns — the remote shell keeps running throughout.

## Install

```bash
cargo install etr
```

Both `etr` (client) and `etrs` (server) are installed. The server must be reachable in PATH on the remote host (true automatically when both machines use `cargo install`).

**Without post-quantum crypto** (smaller binary):

```bash
cargo install etr --no-default-features
```

## Build from source

```bash
git clone https://github.com/l1a/etr
cd etr
cargo build --release
# binaries at target/release/etr and target/release/etrs
```

## Usage

```
etr [OPTIONS] [TARGET]

Arguments:
  [TARGET]  Remote host (e.g. user@host or host)

Options:
  -s, --ssh-port <PORT>        SSH port [default: 22]
  -v, -vv, -vvv                Verbosity: connection events / cipher details / packet trace
      --server-path <PATH>     Path to etrs on the remote host [default: etrs]
      --completions <SHELL>    Print shell completions [bash, zsh, fish, nushell, ...]
```

Verbose logs go to `~/.local/state/etr/etr.log` during a live session (to avoid corrupting the terminal display). Watch with `tail -f ~/.local/state/etr/etr.log`.

## Cryptography

| Suite | KEM | AEAD | KDF |
|-------|-----|------|-----|
| 1 (default) | ML-KEM-1024 | AES-256-GCM | HKDF-SHA3-256 |
| 2 | ML-KEM-768 | AES-256-GCM | HKDF-SHA-256 |
| 3 | X25519 | AES-256-GCM | HKDF-SHA-256 |
| 4 | X25519 | ChaCha20-Poly1305 | HKDF-SHA-256 |

The client advertises all supported suites and the server selects the strongest mutual option. The passkey (generated fresh each session and sent to the server via the SSH-encrypted bootstrap) provides pre-authentication; the KEM provides forward secrecy.

## Reconnect behaviour

- Client detects a dropped connection after **15 seconds** of missed heartbeats.
- Server keeps session state (PTY, cipher, stream history) alive for **30 minutes**.
- The client retries the handshake with the same session ID; the server replays any unacknowledged data.
- A new source IP/port is fine — the session is keyed by session ID, not address.

## Shell completions

```bash
etr --completions zsh > ~/.zfunc/_etr
etr --completions bash > /etc/bash_completion.d/etr
etr --completions fish > ~/.config/fish/completions/etr.fish
etr --completions nushell | save completions-etr.nu
```

## Limitations

- Linux only (PTY layer tested on Linux; macOS/Windows untested)
- Sessions are not persistent across client reboots — the session ID and passkey are in-memory only
- Port forwarding (`-L`/`-R`) is not yet implemented

## License

GPL-3.0-only
