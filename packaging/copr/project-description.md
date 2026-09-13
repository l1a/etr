**etr** is a persistent remote shell in the mould of [Eternal Terminal](https://eternalterminal.dev/) and mosh, written in Rust. The session keeps running on the server when the network drops, and the client reconnects transparently when it comes back.

Like mosh — and unlike the original Eternal Terminal, which needs `etserver` already running — etr requires no pre-installed daemon on the server. The client SSHes to the host, starts `etrs` on the fly, and then connects to it over **QUIC** (TLS 1.3) with the server's certificate pinned, SSH-host-key style. The SSH connection closes immediately afterwards.

The server binds an ephemeral UDP port per session (`etrs -p` pins one if your firewall needs a fixed rule), and the client learns which port over the SSH bootstrap.

Scrollback, full-screen TUI applications and terminal resizing all behave as they do over SSH, because the remote side is a real PTY. Local and remote TCP/UDP port forwarding (`-L`, `-R`) and X11 forwarding (`-X`, `-Y`) are supported, and `-4`/`-6` express an address-family preference rather than a restriction.

This project is an independent Rust reimplementation. It is not affiliated with the original Eternal Terminal project.
