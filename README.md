# etr

A Rust reimplementation of [Eternal Terminal](https://eternalterminal.dev/) (`et`) — a remote shell that automatically reconnects without interrupting your session.

Unlike SSH, when your network drops, `etr` keeps the remote shell alive and transparently reconnects when connectivity returns. Like mosh, no server daemon needs to be pre-installed or running: `etr` bootstraps a per-session server process via SSH, then hands off to a persistent [QUIC](https://www.rfc-editor.org/rfc/rfc9000) connection.

## Quick start

```bash
# Install
cargo install etr

# Connect
etr user@host

# Connect on a non-standard SSH port
etr -s 2222 user@host

# Port forward (survives network interruptions)
etr -L 5432:db-host:5432 user@jumphost
```

The only requirement on the server is that `etrs` is in PATH (installed alongside `etr` by `cargo install`).

## How it works

1. `etr` SSHes to the server and starts `etrs`, which generates an ephemeral self-signed TLS certificate, binds a random QUIC port, writes `PORT <n> CERT <fingerprint>` to stdout, and forks into the background. The SSH connection then closes.
2. `etr` connects to that QUIC port with the pinned certificate (analogous to SSH host-key pinning — no CA needed). A `SessionOpen` message authenticates with the passkey that was shared over SSH.
3. All terminal I/O and port-forward traffic flows over QUIC streams. If the connection drops, `etr` reconnects automatically and the server replays any unacknowledged data — the remote shell keeps running throughout.

## Install

```bash
cargo install etr
```

Both `etr` (client) and `etrs` (server) are installed. The server must be reachable in PATH on the remote host (true automatically when both machines use `cargo install`).

## Build from source

```bash
git clone https://github.com/l1a/etr
cd etr
cargo build --release
# binaries at target/release/etr and target/release/etrs

# Run tests
just test             # runs unit/integration tests
just e2e-local        # runs end-to-end tests (requires tmux and local ssh)

# Run benchmarks
just bench            # runs performance benchmarks via Criterion
```

## Usage

```
etr [OPTIONS] [TARGET]

Arguments:
  [TARGET]  Remote host (e.g. user@host or host)

Options:
  -s, --ssh-port <PORT>          SSH port [default: 22]
  -L <[local_port:]host:port[/udp]>
                                 Forward a local port to a remote address (repeatable)
  -R <[remote_port:]host:port[/udp]>
                                 Forward a remote port to a local address (repeatable)
  -v, -vv, -vvv                  Verbosity: connection events / QUIC details / stream trace
      --server-path <PATH>       Path to etrs on the remote host [default: etrs]
      --log-path <PATH>          Path to the client log file [default: $XDG_STATE_HOME/etr/etr.log]
      --server-log-path <PATH>   Path to the server log file on the remote host [default: $XDG_STATE_HOME/etr/etrs.log]
      --completions <SHELL>      Print shell completions [bash, zsh, fish, nushell, ...]
```

Verbose logs go to `~/.local/state/etr/etr.log` by default during a live session (to avoid corrupting the terminal display), or to the path specified via `--log-path` / config `log_path`. Watch with `tail -f ~/.local/state/etr/etr.log`.

## Transport and security

`etr` uses **QUIC** (via [quinn](https://crates.io/crates/quinn)) which provides:

- **TLS 1.3** — all data is encrypted; the server's ephemeral certificate is pinned via SSH (no CA, no PKI)
- **Reliable ordered delivery** — no dropped or reordered packets reach the application
- **Multiplexed streams** — PTY and each port-forward run on independent QUIC streams; a slow forward cannot stall the terminal
- **Congestion control** — built-in; no hand-rolled flow control needed

## Reconnect behaviour

- Client detects a dropped connection after **15 seconds** of missed heartbeats.
- Server keeps session state (PTY, stream history) alive for **30 minutes**.
- On reconnect the client sends its last-received sequence numbers; the server replays any unacknowledged data.
- A new source IP/port is fine — the session is keyed by session ID and passkey, not address.

## Port forwarding

Local forwarding (`-L`) connects a local port to a remote host:
```bash
# Forward local port 5432 to db-host:5432 via jumphost (TCP, default)
etr -L 5432:db-host:5432 user@jumphost

# UDP forwarding
etr -L 5353:8.8.8.8:53/udp user@jumphost

# Explicit bind address (e.g. wildcard * or specific IP)
etr -L *:8080:localhost:80 user@host
```

Reverse forwarding (`-R`) connects a remote port on the server to a local host:
```bash
# Forward remote port 8080 to local localhost:80 (TCP, default)
etr -R 8080:localhost:80 user@host

# UDP reverse forwarding
etr -R 5353:127.0.0.1:53/udp user@host

# Reverse forwarding with explicit bind address
etr -R 0.0.0.0:8080:localhost:80 user@host
```

By default, listeners bind to loopback addresses (`127.0.0.1` and `[::1]`). You can use the `-g`/`--gateway-ports` flag to automatically bind all local forwarded ports to wildcard interfaces (`0.0.0.0` and `[::]`), or specify an explicit bind address as the first component of the forward specification.

Multiple `-L` and `-R` specifications can be mixed in a single session.

Port forwards survive the same reconnect cycle as the PTY session. Each TCP connection gets its own QUIC stream; UDP uses a dedicated QUIC stream per forward spec.

## Shell completions

Both `etr` and `etrs` support `--completions <shell>` (bash, zsh, fish, elvish, power-shell, nushell).

```bash
# zsh
etr --completions zsh > ~/.zfunc/_etr
etrs --completions zsh > ~/.zfunc/_etrs

# bash
etr --completions bash > /etc/bash_completion.d/etr
etrs --completions bash > /etc/bash_completion.d/etrs

# fish
etr --completions fish > ~/.config/fish/completions/etr.fish
etrs --completions fish > ~/.config/fish/completions/etrs.fish

# nushell
etr --completions nushell | save completions-etr.nu
etrs --completions nushell | save completions-etrs.nu
```

## Configuration

Optional TOML config at `~/.config/etr/config.toml`:

```toml
[client]
ssh_port = 22                        # default SSH port
server_path = "/usr/local/bin/etrs"      # path to etrs on remote hosts
log_path = "/tmp/client.log"         # path to the client log file
server_log_path = "/tmp/server.log"   # path to the server log file on remote host
```

## Limitations

- Linux and macOS supported (Windows untested)
- macOS binaries (`macos-aarch64`) are published on each release
- Sessions are not persistent across client reboots — the session ID and passkey are in-memory only
- Post-quantum key exchange (ML-KEM) is not yet implemented; standard TLS 1.3 uses X25519 ECDH

## License

GPL-3.0-only
