**etr** is a persistent remote shell in the mould of [Eternal Terminal](https://eternalterminal.dev/) and mosh, written in Rust. The session keeps running on the server when the network drops, and the client reconnects transparently when it comes back.

Unlike mosh, etr needs no pre-running daemon and no UDP port opened in advance: the client SSHes to the host, starts `etrs` on the fly, and then connects to it over **QUIC** (TLS 1.3) with the server's certificate pinned, SSH-host-key style. The SSH connection closes immediately afterwards.

Scrollback, full-screen TUI applications and terminal resizing all behave as they do over SSH, because the remote side is a real PTY. Local and remote TCP/UDP port forwarding (`-L`, `-R`) and X11 forwarding (`-X`, `-Y`) are supported, and `-4`/`-6` express an address-family preference rather than a restriction.

This project is an independent Rust reimplementation. It is not affiliated with the original Eternal Terminal project.
