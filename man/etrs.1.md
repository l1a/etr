# NAME

etrs - Eternal Terminal server (per-session)

# SYNOPSIS

**etrs** \[*OPTIONS*\]

# DESCRIPTION

**etrs** is the per-session server component of **etr**(1). It is not
normally invoked directly; **etr** starts it automatically via SSH during
session bootstrap.

When started, **etrs** reads a header line from stdin in the form:

    SESSION_ID_HEX/PASSKEY/TERM

followed by zero or more optional lines carrying environment variables
(*KEY*=*VALUE*), a remote command (**ETRCMD:**), an X11 request (**ETRX11:**)
and the client's address-family preference (**ETRPREFER:**). Lines **etrs**
does not recognise are ignored, so a newer client can be used with an older
server.

It then generates an ephemeral self-signed TLS certificate, binds a QUIC
port, prints **PORT** *n* **CERT** *cert_hex* to stdout, and forks. The
parent process exits immediately (allowing the SSH connection to close
cleanly). The child detaches from stdio, redirects stderr to its log file,
and enters the session loop.

The session loop waits up to **60 seconds** (configurable via
**\-\-connect\-timeout**) for the client's first authenticated QUIC connection,
then keeps the PTY and shell alive across reconnects for up to **96 hours**
(**4 days** / **345600** seconds by default; **0** disables the timeout so the
session stays alive indefinitely). If the client reconnects, it presents the
same session ID and passkey to resume. When the client cleanly disconnects, a
timeout expires, or **SIGUSR1** arrives while disconnected (past
**\-\-signal\-timeout**), the child exits. Use **\-\-list** and **\-\-clean**
to inspect or safely terminate disconnected sessions without disturbing active
connections.

# OPTIONS

**-p**, **\-\-port** *PORT*
:   QUIC port to bind. Defaults to **0** (OS-assigned random port).

**-b**, **\-\-bind** *ADDR*
:   IP address to bind. Defaults to **[::]** (dual-stack, all interfaces),
    or to **0.0.0.0** when **-4** is given.

**-4**, **\-\-prefer-ipv4**
:   Prefer IPv4: bind **0.0.0.0** unless **-b** says otherwise, and try IPv4
    first when resolving the targets of the client's **-L** forwards.

**-6**, **\-\-prefer-ipv6**
:   Prefer IPv6 when resolving the targets of the client's **-L** forwards.
    The default bind is already the dual-stack **[::]**, which accepts IPv4
    clients too, so this does not change it.

    Both are preferences rather than restrictions: the other family is still
    used when a target has no address of the preferred one. A connecting
    client's own **-4**/**-6** (sent as **ETRPREFER:**) overrides these for
    forward targets, since the forwards are the client's. Config file
    equivalent: **address_family** under **\[server\]**. The two flags are
    mutually exclusive.

**-v**
:   Increase verbosity. May be repeated up to three times:

    **-v** — session lifecycle events (connect, disconnect, timeout)

    **-vv** — QUIC details and session ID

    **-vvv** — per-stream trace

    Output is written to **\$XDG_STATE_HOME/etr/etrs.log** (default:
    **~/.local/state/etr/etrs.log**) after the child forks and detaches.

**\-\-log-path** *PATH*
:   Path to the server log file. Defaults to **\$XDG_STATE_HOME/etr/etrs.log**.

**\-\-reconnect\-timeout** *SECS*
:   How long (in seconds) to keep a session alive while the client is
    disconnected. Set to **0** to stay alive indefinitely. Defaults to
    **345600** (96 hours / 4 days). Override priority: server CLI flag >
    client bootstrap override (**etr \-\-reconnect\-timeout** / **\[client\]**) >
    **ETR_SERVER_NETWORK_TMOUT** > **reconnect_timeout** under **\[server\]**
    in config file > default.

**\-\-connect\-timeout** *SECS*
:   How long (in seconds) to wait for the client's first authenticated QUIC
    connection after SSH bootstrap before shutting down. Set to **0** to
    disable the initial timeout. Defaults to **60**. Override priority:
    server CLI flag > client bootstrap override (**etr \-\-connect\-timeout** /
    **\[client\]**) > **ETR_SERVER_CONNECT_TMOUT** > **connect_timeout** under
    **\[server\]** in config file > default.

**\-\-signal\-timeout** *SECS*
:   Minimum number of seconds a session must have been waiting or disconnected
    before **SIGUSR1** terminates it (connected sessions always ignore
    **SIGUSR1**). Defaults to **0** (any disconnected session exits on
    **SIGUSR1**). Override priority: CLI flag > **ETR_SERVER_SIGNAL_TMOUT** >
    **signal_timeout** under **\[server\]** in config file > default.

**\-\-list**
:   List live **etrs** sessions recorded under
    **\$XDG_STATE_HOME/etr/sessions/** (auto-pruning stale records from exited
    processes), showing PID, state (**waiting**, **connected**, **disconnected**),
    QUIC port, mode, uptime, peer IP, idle duration, and active timeout, then
    exit.

**\-\-clean**
:   Terminate waiting/disconnected **etrs** sessions safely by sending
    **SIGUSR1** (connected sessions ignore **SIGUSR1**), then exit.

**\-\-older\-than** *SECS*
:   When used with **\-\-clean**, only signal sessions that have been waiting
    or disconnected for at least *SECS* seconds.

**-h**, **\-\-help**
:   Print help and exit.

**-V**, **\-\-version**
:   Print version and exit.

# FILES

**~/.local/state/etr/etrs.log**
:   Server log file (XDG_STATE_HOME honoured). Written after the child
    forks. Follow with **tail -f** to monitor a running session.

**~/.local/state/etr/sessions/**
:   Per-user directory (mode 0700) holding active session metadata files
    (`<pid>.toml`, mode 0600) used by **\-\-list** and **\-\-clean**.

# ENVIRONMENT

**SHELL**
:   Shell to launch for the PTY session. Defaults to **/bin/bash** if unset.

**ETR_SERVER_NETWORK_TMOUT**
:   How long (in seconds, **0** = forever) to keep a session alive while
    disconnected if **\-\-reconnect\-timeout** and a client override are not
    specified.

**ETR_SERVER_CONNECT_TMOUT**
:   How long (in seconds, **0** = forever) to wait for the client's first
    authenticated connection if **\-\-connect\-timeout** and a client override
    are not specified.

**ETR_SERVER_SIGNAL_TMOUT**
:   Minimum seconds disconnected before **SIGUSR1** terminates the daemon, if
    **\-\-signal\-timeout** is not specified.

**XDG_STATE_HOME**
:   Base directory for the log file and session registry. Defaults to
    **~/.local/state**.

# SIGNALS

**SIGTERM**, **SIGHUP**
:   Cleanly shut down the **etrs** session immediately (closing any active
    connection, hanging up the shell process group, and recording a utmp logout).

**SIGUSR1**
:   Terminate the daemon **only** if it is currently waiting or disconnected
    (and has been disconnected for at least **signal_timeout** seconds).
    Connected sessions ignore **SIGUSR1**, making `pkill -USR1 -x etrs` (or
    `etrs --clean`) safe to run from inside an active session.

# SECURITY

**etrs** uses a self-signed TLS certificate generated fresh for each
session. The certificate DER (hex-encoded) is transmitted to the client
over the authenticated SSH channel and pinned — no CA is required. The
QUIC transport uses TLS 1.3 (X25519 key exchange, AES-256-GCM-SHA384 or
ChaCha20-Poly1305).

Incoming QUIC connections must present the correct session ID and passkey
(32-character random alphanumeric string) generated by **etr**(1) at
session start. Connections that fail this check are rejected.

**etrs** also receives reverse port forwarding specifications from the client
via the **SessionOpen** message and binds TCP/UDP listeners for each spec.
By default these listeners bind to **127.0.0.1** and **[::1]** (loopback only).
When the client specifies **-g** / **\-\-gateway-ports**, the listeners bind to
a dual-stack **[::]** socket instead, making them reachable from external
hosts.  Ensure appropriate firewall rules are in place before enabling
gateway-mode reverse forwarding.


# SEE ALSO

**etr**(1), **ssh**(1)

# AUTHORS

The etr project contributors. See the source repository for details.

# LICENSE

GNU General Public License v3.0 or later.
