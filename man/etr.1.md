---
title: ETR
section: 1
header: User Commands
date: June 2026
---

# NAME

etr - Eternal Terminal client

# SYNOPSIS

**etr** \[*OPTIONS*\] \[*user*@\]*host*

**etr** **\-\-completions** *SHELL*

# DESCRIPTION

**etr** connects to a remote host and starts a persistent terminal session
that survives network interruptions. Unlike SSH, the session keeps running
on the server when the connection drops, and **etr** reconnects transparently
when the network comes back.

**etr** requires no pre-running daemon on the server. It bootstraps the
session by SSHing to the remote host, starting **etrs**(1) on the fly, and
then connecting to it over QUIC (TLS 1.3). Certificate pinning is used in
place of a CA: the server certificate is transmitted over the authenticated
SSH channel and pinned for subsequent QUIC connections.

# OPTIONS

*\[user@\]host*
:   Remote host to connect to. May include an optional *user* prefix
    (e.g. **alice@example.com**). If omitted, help is printed.

**-s**, **\-\-ssh-port** *PORT*
:   SSH port to use for the initial bootstrap connection. Defaults to 22,
    or the value of **ssh_port** in the config file.

**-v**
:   Increase verbosity. May be repeated up to three times:

    **-v** — connection lifecycle events

    **-vv** — QUIC details and session ID

    **-vvv** — per-stream trace

    When running interactively, verbose output is written to
    **\$XDG_STATE_HOME/etr/etr.log** (default: **~/.local/state/etr/etr.log**)
    to avoid corrupting the raw-mode terminal display. Before the session
    enters raw mode, output goes to stderr.

**-L** \[*bind_address*:\]*local_port*:*remote_host*:*remote_port*\[/*tcp*|/*udp*\]
:   Forward a local port to a remote destination, similar to **ssh -L**.
    The protocol defaults to TCP if not specified. By default, the local listener
    binds to both **127.0.0.1** and **[::1]** loopback interfaces.

    If *bind_address* is specified as **\***, **0.0.0.0**, or **::**, the listener binds
    a single dual-stack **[::]** socket, which accepts both IPv4 and IPv6 connections.
    Alternatively, it can bind to a specific interface IP.
    The **-g** / **\-\-gateway-ports** flag can also be used to bind all forwards to all interfaces.

    This option may be repeated to open multiple forwards concurrently alongside the PTY
    session.

    Examples:

        -L 8080:localhost:80
        -L *:3000:localhost:3000
        -L [::1]:8080:localhost:80
        -L 5353:8.8.8.8:53/udp
        -L 5432:db.internal:5432/tcp

**-R** \[*bind_address*:\]*remote_port*:*local_host*:*local_port*\[/*tcp*|/*udp*\]
:   Forward a remote port on the server to a local destination, similar to **ssh -R**.
    The protocol defaults to TCP if not specified. By default, the remote listener
    on the server binds to both **127.0.0.1** and **[::1]** loopback interfaces.

    If *bind_address* is specified as **\***, **0.0.0.0**, or **::**, the remote listener binds
    to all interfaces on the server. Alternatively, it can bind to a specific interface IP.

    This option may be repeated to open multiple reverse forwards concurrently alongside the PTY
    session.

    Examples:

        -R 8080:localhost:80
        -R 0.0.0.0:3000:localhost:3000
        -R 5353:127.0.0.1:53/udp

**-g**, **\-\-gateway-ports**
:   Allow remote hosts to connect to local forwarded ports. Similar to **ssh -g**.
    Binds local forwarded ports to a single dual-stack **[::]** socket, which
    accepts both IPv4 and IPv6 connections on all interfaces.


**\-\-server-path** *PATH*
:   Path to the **etrs** binary on the remote host. Defaults to **etrs**
    (relies on **PATH**), or the value of **server_path** in the config file.

**\-\-log-path** *PATH*
:   Path to the client log file. Defaults to **\$XDG_STATE_HOME/etr/etr.log**.

**\-\-server-log-path** *PATH*
:   Path to the server log file on the remote host. Defaults to **\$XDG_STATE_HOME/etr/etrs.log**.

**\-\-completions** *SHELL*
:   Print shell completion script for the given shell and exit. Supported
    shells: **bash**, **zsh**, **fish**, **elvish**, **powershell**, **nushell**.

**-h**, **\-\-help**
:   Print help and exit.

**-V**, **\-\-version**
:   Print version and exit.

# CONFIGURATION

**etr** reads a TOML config file from
**\$XDG_CONFIG_HOME/etr/config.toml** (default: **~/.config/etr/config.toml**).
All fields are optional. CLI flags take precedence over config values.

```toml
[client]
# Default SSH port (default: 22)
ssh_port = 22

# Path to etrs on remote hosts (default: "etrs", relies on PATH)
server_path = "/usr/local/bin/etrs"

# Default path to the client log file
log_path = "/tmp/client.log"

# Default path to the server log file on the remote host
server_log_path = "/tmp/server.log"

# Allow remote hosts to connect to local forwarded ports (default: false)
gateway_ports = true

# Default local port forwards
forward = ["8080:localhost:80", "*:3000:localhost:3000"]

# Default remote port forwards
reverse_forward = ["9090:localhost:90"]
```

# FILES

**~/.config/etr/config.toml**
:   Client configuration file (XDG_CONFIG_HOME honoured).

**~/.local/state/etr/etr.log**
:   Client verbose log file written during raw-mode sessions
    (XDG_STATE_HOME honoured).

# ENVIRONMENT

**TERM**
:   Passed to the remote shell via the bootstrap protocol. Defaults to
    **xterm-256color** if unset.

**SHELL**
:   Not used by **etr** itself; used by **etrs**(1) on the server to
    determine which shell to launch.

**XDG_CONFIG_HOME**
:   Base directory for the config file. Defaults to **~/.config**.

**XDG_STATE_HOME**
:   Base directory for the log file. Defaults to **~/.local/state**.

# EXAMPLES

Connect to a remote host:

    etr user@example.com

Connect on a non-standard SSH port:

    etr -s 2222 user@example.com

Connect with verbose logging:

    etr -vv user@example.com

Forward a local port to a remote database:

    etr -L 5432:db.internal:5432 user@example.com

Forward a UDP port (DNS) through a jump host:

    etr -L 5353:8.8.8.8:53/udp user@jumphost

Reverse forward a remote port on the server to a local web server:

    etr -R 8080:localhost:80 user@example.com

Reverse forward allowing external connections to the server's port:

    etr -R 0.0.0.0:8080:localhost:80 user@example.com

Generate zsh completions:

    etr --completions zsh > ~/.zfunc/_etr

# EXIT STATUS

**0**
:   Clean disconnect.

**non-zero**
:   Connection or session error.

# SEE ALSO

**etrs**(1), **ssh**(1), **ssh_config**(5)

# BUGS

- Session state is not persisted; re-attaching from a different client
  machine is not supported.


# AUTHORS

The etr project contributors. See the source repository for details.

# LICENSE

GNU General Public License v3.0 or later.
