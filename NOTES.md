# etr — Project Status Notes

Standing project state for **etr**: architecture invariants, current status, known gaps, hard-won lessons, and test coverage.
**Not a changelog:** History lives in `git log` and GitHub Releases (pre-v0.10.10 log dropped in v0.10.10). Wire details live in `PROTOCOL.md`; user docs in `README.md` / wiki; release & packaging runbooks in `docs/RELEASE.md`.

---

## Current state: v0.11.5

`main` carries **0.11.5** (newest released tag **`v0.11.2`** across GitHub, crates.io, AUR, COPR, Homebrew, and Scoop).

- **Working scope:** Full round trip works on Linux and macOS (SSH bootstrap, QUIC pinned-cert session, PTY, keepalives, seamless reconnect, `-L`/`-R` TCP & UDP forwarding, X11 forwarding, `-4`/`-6` family preference). Windows client works (`etrs` is Unix-only by design). **182 tests** (178 `etr` + 4 `tools/stress`).
- **Security hardening (`v0.10.18`):** Validates server `StreamOpen` vs `-X`/`-Y` and `-R` specs (`is_authorized_reverse_stream`); `-X` generates untrusted X11 `SECURITY` cookies (`0700` dir) and exact-display `parse_xauth_list_for_display`; atomic free X11 socket bind (`0600`, no `remove_file` TOCTOU); 10s QUIC handshake & `SessionOpen` timeouts; constant-time passkey/cookie compares (`constant_time_eq`); `utmp` writes only post-auth; filters newline/NUL and linker/shell-init env vars (`LD_PRELOAD`, `BASH_ENV`, etc.); caps concurrent UDP senders (`MAX_UDP_SENDERS = 256`); `0600`/`O_NOFOLLOW` logs (`0700` state dir) with keystroke redaction at `-vvv`.
- **UDP stress baseline (`v0.10.15`):** Deadline-advancing busy-wait `Pacer` (`SPIN_HORIZON = 250 us`) measures loopback 1400-byte echo zero-loss knee at **~112 Mb/s** (`100 us` step; loss starts at `75 us` / `149 Mb/s`; saturates ~205–217 Mb/s echo / ~410–434 Mb/s bidirectional).
- **CI & Secrets:** Hardened `Claude Code Review` workflow (`v0.11.3`, `docs/RELEASE.md`). COPR token expires **2027-02-27** (renew CLI config + repo secrets together).

---

## Architecture & Config Invariants
*(Full connection diagram, protobuf framing, and CLI flags live in `PROTOCOL.md`, `README.md`, and `etr --generate-config`.)*

- **Bootstrap & Transport:** `etr` runs `ssh -p <port> <target> etrs` (`session_id_hex/passkey/term\n` on stdin); `etrs` generates an ephemeral self-signed TLS cert, binds QUIC port `0`, prints `PORT <n> CERT <cert_der_hex>`, and forks (parent exits so SSH closes; child calls `setsid()` and writes `~/.local/state/etr/etrs.log`). Client pins cert DER over QUIC (`quinn 0.11`, TLS 1.3 X25519).
- **Timeouts & Persistence:** Default `connect_timeout = 60` s, `reconnect_timeout = 345600` s (96 h; `0` = forever), `signal_timeout = 0` (`SIGUSR1` / `etrs --clean` kills only waiting/disconnected sessions past timeout; connected sessions drain/ignore it). `send_history` is byte-capped at **4 MB/stream** (oldest-first eviction + 5s `Heartbeat.last_received_seq` trimming).
- **Stream tags:** `0x01` Control (`[4B len][proto]`), `0x02` PTY (`[8B seq BE][4B len][data]`), `0x03` Forward (TCP: `[4B len][StreamOpen]` + raw bytes; UDP: `[4B len][UdpDatagram]` with peer addr/port).
- **Logging & Addressing:** Client `-v`/`-vv`/`-vvv` logs interactively to `~/.local/state/etr/etr.log` (never stderr in raw mode). `etrs` default bind is `[::]` (`0.0.0.0` under `-4`). `-4`/`-6` (`address_family`) are **preferences** with fallback (`--prefer-ipv4`/`--prefer-ipv6`), unlike `ssh -4`/`-6`. Config lives at `$XDG_CONFIG_HOME/etr/config.toml` (`etr --generate-config` / `--merge-config`).

---

## Known gaps / next steps
*Live items only — delete finished items rather than striking through.*

### Correctness
- **Clean shell `exit` reconnecting instead of quitting (observed once Windows->WSL, unreproduced in 24 trials incl. 300ms delay):** Teardown-race hypothesis disproven (`pty_writer_task` keeps channel alive); likely a real-network drop missing `etrs`'s 1s pending-client window. **Do not attempt a fix without `etr -vvv` + `etrs` logs from an actual occurrence.** (`~.` still restores terminal.)

### Measurement
- **Concurrent multi-gigabit TCP load drops UDP forwards to ~41/100 Mb/s offered** (unmeasured whether contention or forwarder limit).
- **MTU awareness (`quinn` `upper_bound` 1452):** Raising globally measured **28% slower** on loopback; only revisit if `quinn` MTU discovery improves, gated to jumbo/LAN paths and re-measured one-way.

### Deliberately out of scope
- **PQC key exchange** (retired with ML-KEM; re-addable via `rustls-post-quantum` when stable).
- **Wayland forwarding/proxying** and **X11 forwarding on Windows** (no Unix domain sockets; fails cleanly at startup).
- **`etrs` on Windows** (needs `fork`/`setsid`; client/completions build cleanly, session run prints clear error).

---

## Hard-won lessons

### 1. Verification — Oracles That Answer a Different Question
- **Watch every guard fail from a clean baseline** (`--self-test` sabotage controls must assert match count > 0 and never run against pre-broken `HEAD`). Strip comments first (`code_of()` in `gate_conformance.py`).
- **A `Cargo.lock` test inside `cargo test` cannot fail** (`cargo test` re-resolves and rewrites `Cargo.lock` before running). Pin vulnerable-version floors in **`Cargo.toml`** instead.
- **Behavioural repros must reach the real failure mode:** Stall-the-reader passed on vulnerable `quinn 0.11.15` because GRO batching is required to hit the chunk-gap bug.
- **Test shipped functions, not re-implemented test loops** (`parse_bootstrap_line`, `effective_bind_ip`).
- **Process/recipe traps:** `pkill` takes ~0.3s for `etrs` to exit (wait 4s in negative controls); under `set -e`, read non-zero exits via `CMD || RC=$?`; prove extraction boundaries when testing partial recipes; `man-check` compares `HEAD` with `HEAD` (never worktree, or uncommitted bumps fail); `git diff --quiet` misses untracked files (`git diff --cached` after staging); test speculated bottlenecks before writing them down (3,600x off on "UDP protobuf bottleneck").

### 2. Testing Conventions
- **Snapshot & reap only newly spawned PIDs** (`scripts/e2e_procs.sh`) — never `pkill -f` (matches own shell, `~/AGENTS.md` §5).
- **Run local E2E/stress against `./target/{debug,release}/etr`**, never `just install` (`~/AGENTS.md` §2).
- **Use `pty.fork()` with `</dev/null`** for stdin-EOF E2E tests (tmux PTYs never EOF). Detach via `os.setsid()` in Python (`setsid(1)` absent on macOS).
- **Strip `git rev-parse --local-env-vars`** and isolate `GIT_CONFIG_{NOSYSTEM,GLOBAL}` + `--no-gpg-sign --no-verify` in `--self-test` fixtures (`pre-push` hook inherits `GIT_DIR` etc.).
- **Windows `Path::canonicalize` verbatim (`\\?\`) trap:** `PathBuf::join("..")` normalizes `..` in-place on verbatim prefixes, erasing `ParentDir` in traversal tests -> canonicalize `temp_dir()` on Unix only.

### 3. Transport & Runtime
- **`quinn` `stream_receive_window / frame_payload` must stay under chunk cap (1024):** Pin `quinn = "0.11.12"` (with `quinn-proto 0.11.18+` self-limiting fix); `INTERNAL_ERROR: too many gaps` had 0 UDP kernel drops (caused by TCP `write_all` backpressure, not packet loss; buffer tweaks only delayed failure).
- **Measure throughput one-way (`just throughput-local`/`throughput-remote`)**, not echo (which counts offered load twice), and run control vs branch back-to-back with identical binaries (no `-C target-cpu=native` across hosts -> `SIGILL`). Don't chase 1452-byte loopback numbers vs 65536-byte TCP. Use multi-accept `tcp-sink` (`ncat -z` consumes a one-shot sink).
- **PTY EOF & Tokio `spawn_blocking` deadlock:** Relay stdin EOF in-band as `VEOF` (`0x04`) when `has_remote_command`. Because PTY readers hold a master clone, closing never sends HUP to commands — send `SIGHUP` to child pgrp escalating to `SIGKILL` after 1s on loop exit.
- **Clean teardown:** Never `std::process::exit()` (`X11Cleanup::drop` leaks `/tmp/.X11-unix/X<n>` + xauth); guard `restore_terminal` on `RAW_EVER_ENABLED` (prevents 70B VT escapes corrupting piped stdout); record `utmp` logout on reconnect-expiry exit too, using `peer.ip().to_canonical()` (avoids `::ffff:...`).
- **Zombie PIDs & `<pid>.toml` ordering (`v0.11.0`/`v0.11.2`):** `kill(pid, 0)` reports unreaped zombies (`bwrap` PID 1) as live -> reject `Z`/`X` in `/proc/<pid>/stat` (`ps -o state=` off Linux). Write `<pid>.toml` *after* registering `SIGUSR1`/`SIGTERM`/`SIGHUP`. `etrs --clean` waits up to `3.0 s` (> 1s `SIGHUP->SIGKILL` + `utempter`).
- **Port validation & `-4`/`-6`:** Validate `dg.peer_port` cleanly (`u16` truncation sent port 65536 to 0). Resolve DNS before invoking `ssh -4`/`-6` (ssh hard-fails on missing family whereas `etr` falls back; TCP forwarding uses `forward::connect_tcp_preferred`).
- **Windows console input:** `std::io::stdin` (`ReadConsoleW`) drops non-UTF-8 -> use `ReadFile` with `ENABLE_VIRTUAL_TERMINAL_INPUT` + UTF-8 codepage, gated on raw-mode init signal (a `ReadFile` started in cooked mode stays line-buffered), and restore original modes/codepage on exit.
- **`build.rs`:** Scan `/usr/lib/*/` for `libutempter` (not `x86_64-linux-gnu` only -> broke aarch64 release builds).

### 4. Release, Packaging & Tooling
- **Cross-repo drift:** Always propagate Part 1 / shared Justfile & workflow fixes across `etr`, `retch`, `rusticprofile`, and `dog`; track helper `TEMPLATE_VERSION` independently of block markers.
- **`WIP.md` LF & markers:** `reset_wip.py` reads/writes bytes and anchors `<!-- BEGIN/END merge-pr state -->` to full lines; `just wip-check` catches CRLF in gitignored `WIP.md`.
- *(Full 6-channel packaging, AUR clone verification, COPR `HOME=/builddir` + explicit tar rules, `--cleanup=verbatim`, and Claude Code Review specs live in `docs/RELEASE.md` & `~/.agents/docs/ops/rust-ci-packaging.md`.)*

### 5. CI & Claude Code Review Hardening
- **Two-checkout isolation & fork guard:** `claude-code-review.yml` checks out the API-resolved base branch at workspace root and PR head under `.ci-review/head/` with `persist-credentials: false` on both (narrower designs leaked credentials or workspace rules), refuses cross-repo PRs before checkout, and strips `CLAUDE*.md`, `AGENTS.md`, `.claude/`, `.claude.json`, and `.mcp.json` from head.
- **Tool boundary & foreground subagents:** Pin `--model claude-opus-5-5` (Sonnet skips subagents while still reporting clean), `--permission-mode default` with `--allowedTools` scoped solely to inline comments, `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1` (keeps subagents foregrounded so `claude-code-action` does not exit early), and `use_sticky_comment: false` (clean runs post no top-level comment). Interactive `claude.yml` pins `--permission-mode acceptEdits` explicitly so `--allowedTools` remains a strict boundary.

---

## Test coverage (182 tests)
- **`quic`:** window/chunk-cap bounds doc, cert gen, TLS config, Envelope/PTY framing, `constant_time_eq`
- **`protocol`:** `SessionOpen`/`Accept`, `StreamOpen`/`Close`, `Heartbeat`, `Disconnect`, `UdpDatagram`
- **`session/{stream,mod}` / `sessions`:** acks, replays, stream idempotence, duration/table formatting, `SessionRegistryGuard` (`0600`/`0700`/symlink/zombie pruning)
- **`bin/etrs` / `bin/etr` / `login`:** CLI flags, bootstrap `parse_bootstrap_line`, `effective_bind_ip`, timeout precedence, X11 cookies, `is_safe_client_env_var`, `is_authorized_reverse_stream`, terminal reset sequences, `utmp` no-panic
- **`config` / `forward` / `addrfam`:** TOML parse/`merge_defaults`, log file traversal/symlink guards, `UdpFrameEncoder` / `datagram_peer_addr`, `-L`/`-R` specs, `resolve_udp_target`, `connect_tcp_preferred`, X11 display/xauth matching, `AddrPref` ordering
- **`tests/session_timeouts` & `tools/stress`:** E2E daemon connect/SIGUSR1/`--list`/`--clean`/QUIC reconnect expiry, `Pacer` spin-window & lag clamping
