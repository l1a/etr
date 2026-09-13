Enable the repository and install:

```
sudo dnf copr enable kentobias/etr
sudo dnf install etr
```

This installs both binaries: `etr` (the client) and `etrs` (the per-session server).

**Install it on both ends.** `etr` starts `etrs` on the remote host over SSH, so the *server* needs the package too — or at least `etrs` on its `PATH`. If `etrs` lives somewhere unusual there, point at it with `etr --server-path /path/to/etrs`.

Then connect:

```
etr user@host
```

No daemon to start and no firewall port to open in advance: the server binds an ephemeral QUIC port per session and the client is told which one over the SSH bootstrap.

To check it works, drop your network for a minute mid-session — the shell should pick up exactly where it was.

Man pages (`man etr`, `man etrs`) and shell completions for bash, zsh and fish are installed with the package.
