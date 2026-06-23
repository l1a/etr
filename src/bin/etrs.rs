// SPDX-License-Identifier: GPL-3.0-or-later
use clap::{ArgAction, CommandFactory, Parser, ValueEnum};
use clap_complete::Shell;
use clap_complete_nushell::Nushell;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{self, Read, Write};
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

use etr::config::Config;
use etr::login;
use etr::protocol::{
    Disconnect, Envelope, ForwardProto, Payload, SessionAccept, StreamOpen, UdpDatagram,
};
use etr::quic::{self, TAG_CONTROL, TAG_FORWARD, TAG_PTY};
use etr::session::SessionState;

static VERBOSITY: std::sync::OnceLock<u8> = std::sync::OnceLock::new();

fn verbosity() -> u8 {
    *VERBOSITY.get().unwrap_or(&0)
}

macro_rules! vlog {
    ($level:expr, $($arg:tt)*) => {
        if verbosity() >= $level { eprintln!($($arg)*); }
    };
}

#[derive(Parser)]
#[command(
    name = "etrs",
    version = env!("CARGO_PKG_VERSION"),
    about = "Eternal Terminal Server — started per-session by etr via SSH"
)]
struct Cli {
    /// QUIC port to bind (0 = random)
    #[arg(short, long, default_value = "0")]
    port: u16,

    /// IP address to bind
    #[arg(short, long, default_value = "[::]")]
    bind: String,

    /// Verbosity: -v session events, -vv QUIC details, -vvv stream trace
    #[arg(short = 'v', action = ArgAction::Count)]
    verbose: u8,

    /// Path to the server log file (default: $XDG_STATE_HOME/etr/etrs.log)
    #[arg(long, value_name = "PATH")]
    log_path: Option<std::path::PathBuf>,

    /// How long (seconds) to keep a session alive while the client is
    /// disconnected. Override via ETR_SERVER_NETWORK_TMOUT or config file
    /// [server] reconnect_timeout. Default: 1800 (30 min).
    #[arg(long, value_name = "SECS")]
    reconnect_timeout: Option<u64>,

    /// Generate shell completions for the specified shell
    #[arg(long, value_enum, value_name = "SHELL")]
    completions: Option<ShellChoice>,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum ShellChoice {
    Bash,
    Elvish,
    Fish,
    PowerShell,
    Zsh,
    Nushell,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let _ = VERBOSITY.set(cli.verbose);
    let cfg = Config::load();

    // Priority: CLI flag > ETR_SERVER_NETWORK_TMOUT env var > config file > default.
    let reconnect_timeout = Duration::from_secs(
        cli.reconnect_timeout
            .or_else(|| {
                std::env::var("ETR_SERVER_NETWORK_TMOUT")
                    .ok()
                    .and_then(|v| v.parse().ok())
            })
            .or(cfg.server.reconnect_timeout)
            .unwrap_or(1800),
    );

    if let Some(shell) = cli.completions {
        let mut cmd = Cli::command();
        match shell {
            ShellChoice::Bash => {
                clap_complete::generate(Shell::Bash, &mut cmd, "etrs", &mut io::stdout())
            }
            ShellChoice::Elvish => {
                clap_complete::generate(Shell::Elvish, &mut cmd, "etrs", &mut io::stdout())
            }
            ShellChoice::Fish => {
                clap_complete::generate(Shell::Fish, &mut cmd, "etrs", &mut io::stdout())
            }
            ShellChoice::PowerShell => {
                clap_complete::generate(Shell::PowerShell, &mut cmd, "etrs", &mut io::stdout())
            }
            ShellChoice::Zsh => {
                clap_complete::generate(Shell::Zsh, &mut cmd, "etrs", &mut io::stdout())
            }
            ShellChoice::Nushell => {
                clap_complete::generate(Nushell, &mut cmd, "etrs", &mut io::stdout())
            }
        }
        return Ok(());
    }

    // Read session bootstrap line from SSH stdin: SESSION_ID_HEX/PASSKEY/TERM
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();
    let parts: Vec<&str> = input.split('/').collect();
    if parts.len() < 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Expected SESSION_ID_HEX/PASSKEY/TERM on stdin",
        ));
    }
    let session_id = hex_decode(parts[0])
        .and_then(|b| <[u8; 16]>::try_from(b).ok())
        .ok_or_else(|| io::Error::other("invalid session_id hex"))?;
    let passkey = parts[1].to_string();
    let term = parts[2].to_string();

    // Read any extra KEY=VALUE lines the client wrote after the first line.
    // A line prefixed with "ETRCMD:" carries the remote command to run instead
    // of an interactive shell.  Old clients close stdin immediately (zero iters).
    let mut extra_env: Vec<String> = Vec::new();
    let mut remote_command: Option<String> = None;
    let mut x11_enabled = false;
    {
        use std::io::BufRead;
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) if l.is_empty() => break,
                Ok(l) if l.starts_with("ETRCMD:") => {
                    remote_command = l.strip_prefix("ETRCMD:").map(str::to_string);
                }
                Ok(l) if l.starts_with("ETRX11:") => {
                    x11_enabled = true;
                }
                Ok(l) => extra_env.push(l),
                Err(_) => break,
            }
        }
    }

    // Generate ephemeral self-signed QUIC certificate.
    let (cert, key) = quic::generate_self_signed_cert();
    let cert_hex = hex_encode(cert.as_ref());

    // quinn::Endpoint requires a tokio runtime, so we can't create it before
    // forking.  Instead, open a pipe: the child creates the endpoint (getting
    // an OS-assigned port), writes the port back to the parent, then the parent
    // prints "PORT <n> CERT <hex>" to SSH stdout and exits.
    let (owned_r, owned_w) = nix::unistd::pipe().map_err(|e| io::Error::other(e.to_string()))?;
    // Convert to raw fds so they can be used after the fork without ownership tracking.
    let pipe_r: i32 = owned_r.into_raw_fd();
    let pipe_w: i32 = owned_w.into_raw_fd();

    // Fork: parent waits for port from child; child runs the tokio session.
    use nix::unistd::{ForkResult, fork, setsid};
    match unsafe { fork() }.map_err(|e| io::Error::other(e.to_string()))? {
        ForkResult::Parent { .. } => {
            // Close write end; read the 2-byte port the child sends.
            nix::unistd::close(pipe_w).ok();
            let mut port_bytes = [0u8; 2];
            let mut reader = unsafe { std::fs::File::from_raw_fd(pipe_r) };
            reader.read_exact(&mut port_bytes)?;
            let actual_port = u16::from_be_bytes(port_bytes);
            println!("PORT {actual_port} CERT {cert_hex}");
            io::stdout().flush()?;
            return Ok(());
        }
        ForkResult::Child => {
            // Close read end; we'll write the port after binding.
            nix::unistd::close(pipe_r).ok();
            setsid().ok();
        }
    }

    // Child: build the Tokio runtime, create the quinn endpoint, send port to
    // parent, then detach stdio and run the session.
    let bind_str = format!("{}:{}", cli.bind, cli.port);
    let bind_addr: std::net::SocketAddr = bind_str
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?;

    let log_path = cli.log_path.clone().unwrap_or_else(session_log_path);

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let server_cfg = quic::server_config(cert, key).map_err(io::Error::other)?;
            let endpoint =
                quinn::Endpoint::server(server_cfg, bind_addr).map_err(io::Error::other)?;
            let actual_port = endpoint.local_addr()?.port();

            // Send port to parent (2 bytes big-endian), then close the pipe.
            let port_bytes = actual_port.to_be_bytes();
            let mut writer = unsafe { std::fs::File::from_raw_fd(pipe_w) };
            writer.write_all(&port_bytes)?;
            drop(writer); // closes pipe_w

            detach_stdio(&log_path)?;
            run_session(
                endpoint,
                session_id,
                passkey,
                term,
                reconnect_timeout,
                extra_env,
                remote_command,
                x11_enabled,
            )
            .await
        })
}

fn detach_stdio(log_path: &std::path::Path) -> io::Result<()> {
    use nix::unistd::{dup2_stderr, dup2_stdin, dup2_stdout};

    let null_file = std::fs::File::open("/dev/null").ok();

    if let Some(p) = log_path.parent() {
        std::fs::create_dir_all(p).ok();
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .ok();

    if let Some(ref f) = null_file {
        dup2_stdin(f).ok();
        dup2_stdout(f).ok();
    }
    let stderr_src = log_file.as_ref().or(null_file.as_ref());
    if let Some(f) = stderr_src {
        dup2_stderr(f).ok();
    }
    Ok(())
}

fn session_log_path() -> std::path::PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                .join(".local/state")
        })
        .join("etr")
        .join("etrs.log")
}

/// Run the session reconnect loop until the client cleanly disconnects or
/// the reconnect window expires.
#[allow(clippy::too_many_arguments)]
async fn run_session(
    endpoint: quinn::Endpoint,
    session_id: [u8; 16],
    passkey: String,
    term: String,
    reconnect_timeout: Duration,
    extra_env: Vec<String>,
    remote_command: Option<String>,
    x11_enabled: bool,
) -> io::Result<()> {
    vlog!(
        1,
        "[etrs] session {} port={}",
        hex_encode(&session_id),
        endpoint.local_addr()?.port()
    );

    let active_conn = Arc::new(Mutex::new(None));
    let x11_real_cookie: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let mut _cleanup = None;
    let mut display_num = None;

    if x11_enabled {
        if let Some(d) = find_free_display() {
            display_num = Some(d);
            let fake_cookie: [u8; 16] = rand::random();
            let fake_cookie_hex = hex_encode(&fake_cookie);

            let status = std::process::Command::new("xauth")
                .arg("add")
                .arg(format!("localhost/unix:{d}"))
                .arg("MIT-MAGIC-COOKIE-1")
                .arg(&fake_cookie_hex)
                .status();

            let mut xauth_ok = false;
            match status {
                Ok(st) if st.success() => {
                    let status2 = std::process::Command::new("xauth")
                        .arg("add")
                        .arg(format!("localhost:{d}"))
                        .arg("MIT-MAGIC-COOKIE-1")
                        .arg(&fake_cookie_hex)
                        .status();
                    if let Ok(st2) = status2
                        && st2.success()
                    {
                        xauth_ok = true;
                    }
                }
                Ok(st) => {
                    vlog!(
                        1,
                        "[etrs] X11: warning: xauth add returned non-zero status: {st}"
                    );
                }
                Err(e) => {
                    vlog!(1, "[etrs] X11: warning: xauth command failed to run: {e}");
                }
            }

            let socket_path = format!("/tmp/.X11-unix/X{d}");
            std::fs::create_dir_all("/tmp/.X11-unix").ok();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions("/tmp/.X11-unix", std::fs::Permissions::from_mode(0o1777))
                .ok();
            std::fs::remove_file(&socket_path).ok();

            let active_conn_clone = Arc::clone(&active_conn);
            let fake_cookie_vec = if xauth_ok {
                fake_cookie.to_vec()
            } else {
                Vec::new()
            };
            let real_cookie_clone = Arc::clone(&x11_real_cookie);

            match tokio::net::UnixListener::bind(&socket_path) {
                Ok(listener) => {
                    let active_conn = Arc::clone(&active_conn_clone);
                    let fake_cookie = fake_cookie_vec.clone();
                    let real_cookie = Arc::clone(&real_cookie_clone);
                    tokio::spawn(async move {
                        while let Ok((stream, _)) = listener.accept().await {
                            let active_conn = Arc::clone(&active_conn);
                            let fake_cookie = fake_cookie.clone();
                            let real_cookie = Arc::clone(&real_cookie);
                            tokio::spawn(async move {
                                let conn = {
                                    let g = active_conn.lock().await;
                                    g.clone()
                                };
                                let rc = {
                                    let g = real_cookie.lock().await;
                                    g.clone()
                                };
                                if let (Some(c), Some(rc_val)) = (conn, rc)
                                    && let Ok((stream, setup_block)) =
                                        process_x11_setup(stream, &fake_cookie, &rc_val).await
                                {
                                    forward_x11_connection(stream, setup_block, c).await;
                                }
                            });
                        }
                    });
                }
                Err(e) => {
                    vlog!(
                        1,
                        "[etrs] X11: failed to bind Unix socket {socket_path}: {e}"
                    );
                }
            }

            let tcp_port = 6000 + d;
            for bind_ip in &["127.0.0.1", "[::1]"] {
                let bind_addr = format!("{bind_ip}:{tcp_port}");
                match tokio::net::TcpListener::bind(&bind_addr).await {
                    Ok(listener) => {
                        let active_conn = Arc::clone(&active_conn_clone);
                        let fake_cookie = fake_cookie_vec.clone();
                        let real_cookie = Arc::clone(&real_cookie_clone);
                        tokio::spawn(async move {
                            while let Ok((stream, _)) = listener.accept().await {
                                let _ = stream.set_nodelay(true);
                                let active_conn = Arc::clone(&active_conn);
                                let fake_cookie = fake_cookie.clone();
                                let real_cookie = Arc::clone(&real_cookie);
                                tokio::spawn(async move {
                                    let conn = {
                                        let g = active_conn.lock().await;
                                        g.clone()
                                    };
                                    let rc = {
                                        let g = real_cookie.lock().await;
                                        g.clone()
                                    };
                                    if let (Some(c), Some(rc_val)) = (conn, rc)
                                        && let Ok((stream, setup_block)) =
                                            process_x11_setup(stream, &fake_cookie, &rc_val).await
                                    {
                                        forward_x11_connection(stream, setup_block, c).await;
                                    }
                                });
                            }
                        });
                    }
                    Err(e) => {
                        vlog!(1, "[etrs] X11: failed to bind TCP {bind_addr}: {e}");
                    }
                }
            }

            _cleanup = Some(X11Cleanup { display_num: d });
        } else {
            vlog!(1, "[etrs] X11: no free display numbers available");
        }
    }

    // ── PTY setup ────────────────────────────────────────────────────────────
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(io::Error::other)?;

    // With a remote command, run it via the user's shell (from $SHELL, same as
    // SSH does) so the PATH and environment match what the user expects.
    // Falls back to /bin/sh if $SHELL is unset.  Without a command, start the
    // user's default login shell (argv[0]="-zsh" sentinel via new_default_prog()).
    let mut cmd = if let Some(ref rcmd) = remote_command {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut c = CommandBuilder::new(&shell);
        c.arg("-c");
        c.arg(rcmd);
        c
    } else {
        CommandBuilder::new_default_prog()
    };
    if let Some(d) = display_num {
        cmd.env("DISPLAY", format!("localhost:{d}.0"));
    }
    cmd.env("TERM", &term);
    // Signal to shell startup scripts that this is an etr session, analogous
    // to SSH_CONNECTION / SSH_TTY set by OpenSSH.
    cmd.env("ETR_CONNECTION", "1");
    cmd.env("ETR_VERSION", env!("CARGO_PKG_VERSION"));
    // Pass SSH_CONNECTION through so shell startup scripts can detect loopback
    // connections (e.g. "etr localhost") via the client IP in its first field.
    if let Ok(v) = std::env::var("SSH_CONNECTION") {
        cmd.env("SSH_CONNECTION", v);
    }
    // Apply any extra env vars requested by the client (--env / config [client] env).
    for kv in &extra_env {
        if let Some((k, v)) = kv.split_once('=') {
            cmd.env(k, v);
        }
    }
    let mut child = pair.slave.spawn_command(cmd).map_err(io::Error::other)?;

    let master_fd = pair.master.as_raw_fd();
    let mut pty_reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
    let mut pty_writer = pair.master.take_writer().map_err(io::Error::other)?;
    let master = Arc::new(Mutex::new(pair.master));

    // Channel: receive stdin keystrokes from client → write to PTY.
    let (pty_in_tx, mut pty_in_rx) = mpsc::channel::<Vec<u8>>(1000);
    tokio::task::spawn_blocking(move || {
        while let Some(data) = pty_in_rx.blocking_recv() {
            if pty_writer.write_all(&data).is_err() {
                break;
            }
            let _ = pty_writer.flush();
        }
    });

    let session_state = Arc::new(Mutex::new(SessionState::new(session_id, passkey.clone())));

    // outbound_pty_tx: current connection's PTY data channel (None = disconnected).
    let outbound_pty_tx: PtyTx = Arc::new(std::sync::Mutex::new(None));

    // outbound_ctrl_tx: current connection's control envelope channel.
    let outbound_ctrl_tx: CtrlTx = Arc::new(std::sync::Mutex::new(None));

    // PTY reader: forwards PTY output into the session and the active connection.
    {
        let outbound_pty_tx = Arc::clone(&outbound_pty_tx);
        let session_state = Arc::clone(&session_state);
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 4096];
            loop {
                match pty_reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        let seq = {
                            let mut s = session_state.blocking_lock();
                            let st = s.stream_mut(0).expect("stream 0 always exists");
                            let seq = st.next_out_seq;
                            st.next_out_seq += 1;
                            st.record_send(seq, data.clone());
                            seq
                        };
                        if let Some(tx) = outbound_pty_tx.lock().unwrap().clone() {
                            let _ = tx.blocking_send((seq, data));
                        }
                    }
                }
            }
        });
    }

    // Shell-exit signal: set to true when the child shell exits.
    let (shell_exit_tx, shell_exit_rx) = tokio::sync::watch::channel(false);

    // Shell-exit watcher: sends Disconnect on the control stream when the shell exits.
    {
        let outbound_ctrl_tx = Arc::clone(&outbound_ctrl_tx);
        let session_id_copy = session_id;
        let logout_fd = master_fd;
        tokio::task::spawn_blocking(move || {
            let _ = child.wait();
            vlog!(
                1,
                "[etrs] shell exited for {}",
                hex_encode(&session_id_copy)
            );
            // Notify the client and signal shutdown BEFORE record_logout so that
            // the Disconnect reaches the client even if utempter is slow.
            if let Some(tx) = outbound_ctrl_tx.lock().unwrap().clone() {
                let _ = tx.blocking_send(Envelope {
                    payload: Some(Payload::Disconnect(Disconnect {})),
                });
            }
            let _ = shell_exit_tx.send(true);
            if let Some(fd) = logout_fd {
                login::record_logout(fd);
            }
        });
    }

    let active_reverse_listeners = Arc::new(Mutex::new(std::collections::HashSet::new()));

    // ── Reconnect loop ───────────────────────────────────────────────────────
    let reconnect_window = reconnect_timeout;

    use tokio::signal::unix::SignalKind;

    let mut sigterm =
        tokio::signal::unix::signal(SignalKind::terminate()).map_err(io::Error::other)?;
    let mut sighup = tokio::signal::unix::signal(SignalKind::hangup()).map_err(io::Error::other)?;
    // A second pair of listeners that cover the window while handle_connection
    // is running.  Tokio notifies every registered listener for a signal, so
    // a single SIGTERM wakes both; whichever select is currently being polled
    // will win.
    let mut sigterm_conn =
        tokio::signal::unix::signal(SignalKind::terminate()).map_err(io::Error::other)?;
    let mut sighup_conn =
        tokio::signal::unix::signal(SignalKind::hangup()).map_err(io::Error::other)?;

    loop {
        let mut shell_rx = shell_exit_rx.clone();
        let incoming = tokio::select! {
            biased;
            _ = shell_rx.wait_for(|&v| v) => {
                // Give a pending client up to 1 s to connect so it receives a
                // clean Disconnect rather than timing out.
                vlog!(1, "[etrs] shell exited; waiting briefly for any pending client...");
                match tokio::time::timeout(
                    Duration::from_secs(1),
                    endpoint.accept(),
                ).await {
                    Ok(Some(inc)) => inc,
                    _ => {
                        vlog!(1, "[etrs] no pending client, shutting down");
                        break;
                    }
                }
            }
            _ = sigterm.recv() => {
                vlog!(1, "[etrs] SIGTERM received, shutting down");
                if let Some(fd) = master_fd {
                    tokio::task::spawn_blocking(move || login::record_logout(fd)).await.ok();
                }
                break;
            }
            _ = sighup.recv() => {
                vlog!(1, "[etrs] SIGHUP received, shutting down");
                if let Some(fd) = master_fd {
                    tokio::task::spawn_blocking(move || login::record_logout(fd)).await.ok();
                }
                break;
            }
            res = tokio::time::timeout(reconnect_window, endpoint.accept()) => {
                match res {
                    Ok(Some(inc)) => inc,
                    Ok(None) => {
                        vlog!(1, "[etrs] endpoint closed");
                        break;
                    }
                    Err(_) => {
                        vlog!(1, "[etrs] reconnect window expired, shutting down");
                        break;
                    }
                }
            }
        };

        let conn = match incoming.accept() {
            Ok(c) => c,
            Err(e) => {
                vlog!(1, "[etrs] accept error: {e}");
                continue;
            }
        };
        let conn = match conn.await {
            Ok(c) => c,
            Err(e) => {
                vlog!(1, "[etrs] QUIC handshake error: {e}");
                continue;
            }
        };

        // Run handle_connection, but also watch for signals so that
        // `pkill etrs` during an active session is handled promptly.
        let conn_for_sig = conn.clone();
        let (clean, signal_shutdown) = tokio::select! {
            r = handle_connection(
                conn,
                session_id,
                &passkey,
                Arc::clone(&session_state),
                Arc::clone(&outbound_pty_tx),
                Arc::clone(&outbound_ctrl_tx),
                pty_in_tx.clone(),
                Arc::clone(&master),
                master_fd,
                Arc::clone(&active_conn),
                Arc::clone(&active_reverse_listeners),
                shell_exit_rx.clone(),
                Arc::clone(&x11_real_cookie),
            ) => (r, false),
            _ = sigterm_conn.recv() => {
                vlog!(1, "[etrs] SIGTERM during active session, closing connection");
                conn_for_sig.close(quinn::VarInt::from_u32(0), b"server shutdown");
                // Allow Quinn to flush the CONNECTION_CLOSE frame.
                tokio::time::sleep(Duration::from_millis(200)).await;
                (false, true)
            }
            _ = sighup_conn.recv() => {
                vlog!(1, "[etrs] SIGHUP during active session, closing connection");
                conn_for_sig.close(quinn::VarInt::from_u32(0), b"server shutdown");
                tokio::time::sleep(Duration::from_millis(200)).await;
                (false, true)
            }
        };

        *outbound_pty_tx.lock().unwrap() = None;
        *outbound_ctrl_tx.lock().unwrap() = None;
        *active_conn.lock().await = None;

        if clean {
            vlog!(1, "[etrs] client disconnected cleanly, shutting down");
            break;
        }
        if signal_shutdown {
            vlog!(1, "[etrs] shutting down after signal");
            if let Some(fd) = master_fd {
                tokio::task::spawn_blocking(move || login::record_logout(fd))
                    .await
                    .ok();
            }
            break;
        }
    }

    Ok(())
}

type PtyTx = Arc<std::sync::Mutex<Option<mpsc::Sender<(u64, Vec<u8>)>>>>;
type CtrlTx = Arc<std::sync::Mutex<Option<mpsc::Sender<Envelope>>>>;

/// Handle one QUIC connection.  Returns `true` on clean Disconnect.
#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    conn: quinn::Connection,
    session_id: [u8; 16],
    passkey: &str,
    session_state: Arc<Mutex<SessionState>>,
    outbound_pty_tx: PtyTx,
    outbound_ctrl_tx: CtrlTx,
    pty_in_tx: mpsc::Sender<Vec<u8>>,
    master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    master_fd: Option<std::os::unix::io::RawFd>,
    active_conn: Arc<Mutex<Option<quinn::Connection>>>,
    active_reverse_listeners: Arc<Mutex<std::collections::HashSet<String>>>,
    shell_exit_rx: tokio::sync::watch::Receiver<bool>,
    x11_real_cookie: Arc<Mutex<Option<Vec<u8>>>>,
) -> bool {
    let peer = conn.remote_address();
    vlog!(
        1,
        "[etrs] connection from {} session={}",
        peer,
        hex_encode(&session_id)
    );
    if let Some(fd) = master_fd {
        let addr = format!(
            "{} via etr [{}]",
            peer.ip().to_canonical(),
            std::process::id()
        );
        tokio::task::spawn_blocking(move || login::record_login(fd, &addr))
            .await
            .ok();
    }

    // ── Control stream: first stream the client opens ─────────────────────
    let (mut ctrl_send, mut ctrl_recv) = match conn.accept_bi().await {
        Ok(s) => s,
        Err(e) => {
            vlog!(1, "[etrs] accept_bi error: {e}");
            return false;
        }
    };

    let tag = match quic::read_tag(&mut ctrl_recv).await {
        Ok(t) => t,
        Err(e) => {
            vlog!(1, "[etrs] read tag error: {e}");
            return false;
        }
    };
    if tag != TAG_CONTROL {
        vlog!(1, "[etrs] expected control tag, got 0x{tag:02x}");
        return false;
    }

    let session_open = match quic::read_msg(&mut ctrl_recv).await {
        Ok(Some(env)) => match env.payload {
            Some(Payload::SessionOpen(so)) => so,
            _ => {
                vlog!(1, "[etrs] expected SessionOpen");
                return false;
            }
        },
        _ => {
            vlog!(1, "[etrs] failed to read SessionOpen");
            return false;
        }
    };

    // Verify session identity and passkey.
    if session_open.session_id != session_id {
        vlog!(1, "[etrs] session_id mismatch from {peer}");
        return false;
    }
    if session_open.passkey != passkey {
        vlog!(1, "[etrs] passkey mismatch from {peer}");
        return false;
    }

    vlog!(2, "[etrs] session verified peer={peer}");
    vlog!(2, "[etrs] {}", etr::quic::tls_info());

    if session_open.x11_enabled {
        let mut rc = x11_real_cookie.lock().await;
        *rc = Some(session_open.x11_auth_cookie.clone());
    }

    // Collect replays and build SessionAccept.
    let (replays, server_last) = {
        let mut s = session_state.lock().await;
        s.apply_server_acks(&session_open.last_received_seq);
        let replays = s.collect_replays(&session_open.last_received_seq);
        let server_last = s.last_received_map();
        (replays, server_last)
    };

    let session_accept = Envelope {
        payload: Some(Payload::SessionAccept(SessionAccept {
            last_received_seq: server_last,
        })),
    };
    if quic::write_msg(&mut ctrl_send, &session_accept)
        .await
        .is_err()
    {
        return false;
    }

    // Update active connection
    {
        let mut ac = active_conn.lock().await;
        *ac = Some(conn.clone());
    }

    // Spawn reverse port forwarding listeners
    {
        let mut started = active_reverse_listeners.lock().await;
        let gateway = session_open.gateway_ports;
        for spec_str in &session_open.reverse_forwards {
            if !started.contains(spec_str) {
                if let Ok(spec) = etr::forward::ForwardSpec::parse(spec_str) {
                    let active_conn_clone = Arc::clone(&active_conn);
                    match spec.proto {
                        ForwardProto::Tcp => {
                            tokio::spawn(run_tcp_reverse_listener(
                                spec,
                                active_conn_clone,
                                gateway,
                            ));
                        }
                        ForwardProto::Udp => {
                            tokio::spawn(run_udp_reverse_listener(
                                spec,
                                active_conn_clone,
                                gateway,
                            ));
                        }
                    }
                    started.insert(spec_str.clone());
                } else {
                    vlog!(1, "[etrs] failed to parse reverse forward spec: {spec_str}");
                }
            }
        }
    }

    // ── Per-connection channels ───────────────────────────────────────────
    let (pty_ob_tx, mut pty_ob_rx) = mpsc::channel::<(u64, Vec<u8>)>(1000);
    *outbound_pty_tx.lock().unwrap() = Some(pty_ob_tx.clone());

    let (ctrl_ob_tx, mut ctrl_ob_rx) = mpsc::channel::<Envelope>(64);
    *outbound_ctrl_tx.lock().unwrap() = Some(ctrl_ob_tx.clone());

    // If the shell already exited before this connection was accepted, queue a
    // Disconnect now so ctrl_writer_task delivers it as soon as it starts.
    if *shell_exit_rx.borrow() {
        let _ = ctrl_ob_tx.try_send(Envelope {
            payload: Some(Payload::Disconnect(Disconnect {})),
        });
    }

    // Queue replay data ahead of live PTY output.
    if let Some(stream0_replays) = replays.get(&0) {
        for (seq, data) in stream0_replays {
            let _ = pty_ob_tx.try_send((*seq, data.clone()));
        }
    }

    // ── Wait for the PTY stream (second stream the client opens) ──────────
    // Meanwhile, accept incoming streams and dispatch them in a separate task.
    let (pty_stream_tx, pty_stream_rx) =
        tokio::sync::oneshot::channel::<(quinn::SendStream, quinn::RecvStream)>();

    let conn_dispatch = conn.clone();
    let session_state_fwd = Arc::clone(&session_state);
    let mut dispatch_task = tokio::spawn(async move {
        let mut pty_tx = Some(pty_stream_tx);
        loop {
            let (fwd_send, mut fwd_recv) = match conn_dispatch.accept_bi().await {
                Ok(s) => s,
                Err(_) => break,
            };
            let tag = match quic::read_tag(&mut fwd_recv).await {
                Ok(t) => t,
                Err(_) => break,
            };
            match tag {
                TAG_PTY => {
                    if let Some(tx) = pty_tx.take() {
                        let _ = tx.send((fwd_send, fwd_recv));
                    }
                }
                TAG_FORWARD => {
                    let so = match quic::read_msg(&mut fwd_recv).await {
                        Ok(Some(env)) => match env.payload {
                            Some(Payload::StreamOpen(so)) => so,
                            _ => continue,
                        },
                        _ => break,
                    };
                    let proto =
                        ForwardProto::try_from(so.forward_proto).unwrap_or(ForwardProto::Tcp);
                    let ss = Arc::clone(&session_state_fwd);
                    match proto {
                        ForwardProto::Tcp => {
                            tokio::spawn(serve_tcp_forward(so, fwd_send, fwd_recv, ss));
                        }
                        ForwardProto::Udp => {
                            tokio::spawn(serve_udp_forward(so, fwd_send, fwd_recv, ss));
                        }
                    }
                }
                _ => {
                    vlog!(1, "[etrs] unknown stream tag 0x{tag:02x}, ignoring");
                }
            }
        }
    });

    let (mut pty_send, mut pty_recv) = match pty_stream_rx.await {
        Ok(s) => s,
        Err(_) => {
            dispatch_task.abort();
            return false;
        }
    };

    // ── PTY output writer: ob_rx → QUIC PTY send stream ──────────────────
    let mut pty_writer_task = tokio::spawn(async move {
        while let Some((seq, data)) = pty_ob_rx.recv().await {
            vlog!(3, "[etrs] pty→client seq={seq} bytes={}", data.len());
            if quic::write_pty_chunk(&mut pty_send, seq, &data)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // ── PTY input reader: QUIC PTY recv stream → pty_in_tx ───────────────
    let session_r = Arc::clone(&session_state);
    let pty_in_tx2 = pty_in_tx.clone();
    let mut pty_reader_task = tokio::spawn(async move {
        while let Ok(Some((seq, data))) = quic::read_pty_chunk(&mut pty_recv).await {
            vlog!(3, "[etrs] pty←client seq={seq} bytes={}", data.len());
            {
                let mut s = session_r.lock().await;
                if let Some(st) = s.stream_mut(0) {
                    st.next_in_seq = seq + 1;
                }
            }
            let _ = pty_in_tx2.send(data).await;
        }
    });

    // ── Control writer: ctrl_ob_rx → QUIC ctrl send stream ───────────────
    let mut ctrl_writer_task = tokio::spawn(async move {
        while let Some(env) = ctrl_ob_rx.recv().await {
            let is_disconnect = matches!(env.payload, Some(Payload::Disconnect(_)));
            if let Some(Payload::Heartbeat(ref hb)) = env.payload {
                vlog!(3, "[etrs] hb→client acks={:?}", hb.last_received_seq);
            }
            let _ = quic::write_msg(&mut ctrl_send, &env).await;
            if is_disconnect {
                break;
            }
        }
    });

    // ── Control reader: QUIC ctrl recv stream → dispatch ─────────────────
    let master_r = Arc::clone(&master);
    let session_ctrl = Arc::clone(&session_state);
    let mut ctrl_reader_task = tokio::spawn(async move {
        loop {
            match quic::read_msg(&mut ctrl_recv).await {
                Ok(Some(env)) => match env.payload {
                    Some(Payload::TerminalResize(tr)) => {
                        let m = master_r.lock().await;
                        let _ = m.resize(PtySize {
                            rows: tr.rows as u16,
                            cols: tr.cols as u16,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                        vlog!(3, "[etrs] resize {}x{}", tr.cols, tr.rows);
                    }
                    Some(Payload::Disconnect(_)) => return true,
                    Some(Payload::Heartbeat(hb)) => {
                        session_ctrl
                            .lock()
                            .await
                            .apply_server_acks(&hb.last_received_seq);
                        vlog!(3, "[etrs] hb←client acks={:?}", hb.last_received_seq);
                    }
                    _ => {}
                },
                _ => return false,
            }
        }
    });

    // ── Heartbeat task ────────────────────────────────────────────────────
    let hb_ctrl_tx = ctrl_ob_tx.clone();
    let session_hb = Arc::clone(&session_state);
    let mut hb_task = tokio::spawn(async move {
        use etr::protocol::Heartbeat;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let last_received_seq = session_hb.lock().await.last_received_map();
            if hb_ctrl_tx
                .send(Envelope {
                    payload: Some(Payload::Heartbeat(Heartbeat { last_received_seq })),
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // ── Wait for any task to finish ───────────────────────────────────────
    let clean = tokio::select! {
        r = &mut ctrl_reader_task => r.unwrap_or(false),
        _ = &mut pty_writer_task  => false,
        _ = &mut pty_reader_task  => false,
        _ = &mut ctrl_writer_task => false,
        _ = &mut hb_task          => false,
        _ = &mut dispatch_task    => false,
    };

    ctrl_reader_task.abort();
    pty_writer_task.abort();
    pty_reader_task.abort();
    ctrl_writer_task.abort();
    hb_task.abort();
    dispatch_task.abort();

    vlog!(1, "[etrs] connection from {} ended (clean={})", peer, clean);
    clean
}

// ── Forward helpers (server side) ────────────────────────────────────────────

async fn pipe_tcp_quic(
    stream: tokio::net::TcpStream,
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut tcp_r, mut tcp_w) = stream.into_split();

    let mut t1 = tokio::spawn(async move {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match tcp_r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if quic_send.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = quic_send.finish();
    });

    let mut t2 = tokio::spawn(async move {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match quic_recv.read(&mut buf).await {
                Ok(None) | Err(_) => break,
                Ok(Some(0)) => continue,
                Ok(Some(n)) => {
                    if tcp_w.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tcp_w.shutdown().await;
    });

    tokio::select! {
        _ = &mut t1 => {}
        _ = &mut t2 => {}
    }
    t1.abort();
    t2.abort();
}

async fn serve_tcp_forward(
    so: StreamOpen,
    quic_send: quinn::SendStream,
    quic_recv: quinn::RecvStream,
    _session: Arc<Mutex<SessionState>>,
) {
    use tokio::net::TcpStream;

    let addr = format!("{}:{}", so.remote_host, so.remote_port);
    let tcp = match TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(e) => {
            vlog!(1, "[etrs] TCP connect to {addr} failed: {e}");
            let mut qs = quic_send;
            let _ = qs.finish();
            return;
        }
    };
    let _ = tcp.set_nodelay(true);
    vlog!(2, "[etrs] TCP forward → {addr}");
    pipe_tcp_quic(tcp, quic_send, quic_recv).await;
    vlog!(2, "[etrs] TCP forward to {addr} closed");
}

/// Accept connections on a remote TCP port and forward each one back to the client via QUIC.
///
/// `gateway` mirrors the client's `-g` / `--gateway-ports` flag: when `true`, the listener
/// binds a dual-stack `[::]` socket (all interfaces); when `false` it binds the loopback
/// pair `127.0.0.1` + `[::1]`, or whatever the spec's explicit bind address specifies.
async fn run_tcp_reverse_listener(
    spec: etr::forward::ForwardSpec,
    active_conn: Arc<Mutex<Option<quinn::Connection>>>,
    gateway: bool,
) {
    use tokio::net::TcpListener;

    let bind_addrs = spec.get_bind_addresses(gateway);
    let mut listeners = Vec::new();
    for addr in &bind_addrs {
        let target = format!("{addr}:{}", spec.local_port);
        match TcpListener::bind(&target).await {
            Ok(l) => listeners.push(l),
            Err(e) => {
                vlog!(1, "[etrs] reverse TCP bind to {target} failed: {e}");
            }
        }
    }

    if listeners.is_empty() {
        vlog!(
            1,
            "[etrs] cannot bind reverse TCP port {} on any of {:?}",
            spec.local_port,
            bind_addrs
        );
        return;
    }

    vlog!(
        1,
        "[etrs] reverse TCP forwarding listening on port {}",
        spec.local_port
    );

    let run_loop = |listener: TcpListener,
                    active_conn: Arc<Mutex<Option<quinn::Connection>>>,
                    spec: etr::forward::ForwardSpec| async move {
        loop {
            let (tcp_stream, peer) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    vlog!(1, "[etrs] reverse TCP accept error: {e}");
                    break;
                }
            };
            vlog!(2, "[etrs] reverse TCP connection from {peer}");
            let active_conn = Arc::clone(&active_conn);
            let spec = spec.clone();
            tokio::spawn(async move {
                let conn = {
                    let g = active_conn.lock().await;
                    g.clone()
                };
                let conn = match conn {
                    Some(c) => c,
                    None => return,
                };
                let (mut quic_send, quic_recv) = match conn.open_bi().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let env = Envelope {
                    payload: Some(Payload::StreamOpen(StreamOpen {
                        stream_id: 0,
                        stream_type: etr::protocol::StreamType::PortForward as i32,
                        remote_host: spec.remote_host.clone(),
                        remote_port: spec.remote_port as u32,
                        forward_proto: ForwardProto::Tcp as i32,
                    })),
                };
                if quic_send.write_all(&[TAG_FORWARD]).await.is_err() {
                    return;
                }
                if quic::write_msg(&mut quic_send, &env).await.is_err() {
                    return;
                }
                pipe_tcp_quic(tcp_stream, quic_send, quic_recv).await;
            });
        }
    };

    let mut join_handles = Vec::new();
    for listener in listeners {
        join_handles.push(tokio::spawn(run_loop(
            listener,
            Arc::clone(&active_conn),
            spec.clone(),
        )));
    }

    for h in join_handles {
        let _ = h.await;
    }
}

/// Accept datagrams on a remote UDP port and forward each one back to the client via QUIC.
///
/// `gateway` mirrors the client's `-g` / `--gateway-ports` flag: when `true`, the listener
/// binds a dual-stack `[::]` socket (all interfaces); when `false` it binds the loopback
/// pair `127.0.0.1` + `[::1]`, or whatever the spec's explicit bind address specifies.
async fn run_udp_reverse_listener(
    spec: etr::forward::ForwardSpec,
    active_conn: Arc<Mutex<Option<quinn::Connection>>>,
    gateway: bool,
) {
    use tokio::net::UdpSocket;

    let bind_addrs = spec.get_bind_addresses(gateway);
    let mut sockets = Vec::new();
    for addr in &bind_addrs {
        let target = format!("{addr}:{}", spec.local_port);
        match UdpSocket::bind(&target).await {
            Ok(s) => sockets.push(s),
            Err(e) => {
                vlog!(1, "[etrs] reverse UDP bind to {target} failed: {e}");
            }
        }
    }

    if sockets.is_empty() {
        vlog!(
            1,
            "[etrs] cannot bind reverse UDP port {} on any of {:?}",
            spec.local_port,
            bind_addrs
        );
        return;
    }

    vlog!(
        1,
        "[etrs] reverse UDP forwarding listening on port {}",
        spec.local_port
    );

    let mut join_handles = Vec::new();
    for socket in sockets {
        join_handles.push(tokio::spawn(run_udp_reverse_listener_socket(
            socket,
            spec.clone(),
            Arc::clone(&active_conn),
        )));
    }

    for h in join_handles {
        let _ = h.await;
    }
}

/// Drive a single UDP reverse-forward socket: receive datagrams from external senders,
/// multiplex them onto a shared QUIC stream toward the client, and route reply datagrams
/// from the client back to the correct sender using the `peer_addr/peer_port` embedded
/// in each reply envelope.
///
/// One instance of this function runs per bound socket (i.e. per address returned by
/// `spec.get_bind_addresses`).  `active_conn` is shared across all sockets for the same
/// spec so that reconnect does not interrupt the listen loop.
async fn run_udp_reverse_listener_socket(
    socket: tokio::net::UdpSocket,
    spec: etr::forward::ForwardSpec,
    active_conn: Arc<Mutex<Option<quinn::Connection>>>,
) {
    let socket = Arc::new(socket);
    let current_quic_tx: Arc<Mutex<Option<quinn::SendStream>>> = Arc::new(Mutex::new(None));
    let last_conn_id: Arc<std::sync::Mutex<Option<quinn::Connection>>> =
        Arc::new(std::sync::Mutex::new(None));

    let socket_recv = Arc::clone(&socket);
    let active_conn_recv = Arc::clone(&active_conn);
    let current_quic_tx_recv = Arc::clone(&current_quic_tx);
    let last_conn_id_recv = Arc::clone(&last_conn_id);
    let spec_recv = spec.clone();

    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        while let Ok((n, src)) = socket_recv.recv_from(&mut buf).await {
            vlog!(3, "[etrs] UDP reverse fwd received {n} bytes from {src}");

            let conn = {
                let g = active_conn_recv.lock().await;
                g.clone()
            };
            let conn = match conn {
                Some(c) => c,
                None => {
                    vlog!(2, "[etrs] UDP reverse fwd: active_conn is None");
                    continue;
                }
            };

            let mut current_tx = current_quic_tx_recv.lock().await;
            let mut need_new_stream = current_tx.is_none();
            if !need_new_stream {
                let last_c = last_conn_id_recv.lock().unwrap();
                if let Some(ref lc) = *last_c {
                    if lc.stable_id() != conn.stable_id() {
                        need_new_stream = true;
                    }
                } else {
                    need_new_stream = true;
                }
            }

            if need_new_stream {
                vlog!(2, "[etrs] UDP reverse fwd: opening new QUIC stream");
                let (mut tx, rx) = match conn.open_bi().await {
                    Ok(s) => s,
                    Err(e) => {
                        vlog!(1, "[etrs] UDP reverse fwd: open_bi failed: {e}");
                        continue;
                    }
                };
                let env = Envelope {
                    payload: Some(Payload::StreamOpen(StreamOpen {
                        stream_id: 0,
                        stream_type: etr::protocol::StreamType::PortForward as i32,
                        remote_host: spec_recv.remote_host.clone(),
                        remote_port: spec_recv.remote_port as u32,
                        forward_proto: ForwardProto::Udp as i32,
                    })),
                };
                if tx.write_all(&[TAG_FORWARD]).await.is_ok()
                    && quic::write_msg(&mut tx, &env).await.is_ok()
                {
                    *current_tx = Some(tx);
                    *last_conn_id_recv.lock().unwrap() = Some(conn.clone());
                    vlog!(2, "[etrs] UDP reverse fwd: stream opened and header sent");

                    let socket_send = Arc::clone(&socket_recv);
                    let mut rx = rx;
                    tokio::spawn(async move {
                        vlog!(3, "[etrs] UDP reverse fwd: rx reader task started");
                        while let Ok(Some(env)) = quic::read_msg(&mut rx).await {
                            if let Some(Payload::UdpDatagram(dg)) = env.payload
                                && !dg.peer_addr.is_empty()
                                && dg.peer_port > 0
                            {
                                // Parse peer_addr as IpAddr (not SocketAddr) so that bare
                                // IPv6 addresses like "::1" are accepted without brackets.
                                if let Ok(ip) = dg.peer_addr.parse::<std::net::IpAddr>() {
                                    let addr = std::net::SocketAddr::new(ip, dg.peer_port as u16);
                                    vlog!(
                                        3,
                                        "[etrs] UDP reverse fwd rx: sending reply of {} bytes to {addr}",
                                        dg.data.len()
                                    );
                                    let _ = socket_send.send_to(&dg.data, addr).await;
                                }
                            }
                        }
                        vlog!(3, "[etrs] UDP reverse fwd rx: rx reader task ended");
                    });
                } else {
                    vlog!(1, "[etrs] UDP reverse fwd: failed to write tag/header");
                    continue;
                }
            }

            if let Some(ref mut tx) = *current_tx {
                let env = Envelope {
                    payload: Some(Payload::UdpDatagram(UdpDatagram {
                        peer_addr: src.ip().to_string(),
                        peer_port: src.port() as u32,
                        data: buf[..n].to_vec(),
                    })),
                };
                if quic::write_msg(tx, &env).await.is_err() {
                    vlog!(1, "[etrs] UDP reverse fwd: failed to write msg to stream");
                    *current_tx = None;
                } else {
                    vlog!(3, "[etrs] UDP reverse fwd: forwarded datagram of {n} bytes");
                }
            }
        }
    });
}

async fn serve_udp_forward(
    so: StreamOpen,
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
    _session: Arc<Mutex<SessionState>>,
) {
    use std::collections::HashMap;
    use std::time::Instant;
    use tokio::net::UdpSocket;
    use tokio::sync::mpsc;

    // Resolve the remote host once so every per-sender socket uses the same family.
    let remote_addr_str = format!("{}:{}", so.remote_host, so.remote_port);
    let remote_addr: std::net::SocketAddr =
        match etr::forward::resolve_udp_target(&remote_addr_str).await {
            Some(a) => a,
            None => {
                vlog!(1, "[etrs] UDP forward: cannot resolve {remote_addr_str}");
                let _ = quic_send.finish();
                return;
            }
        };
    let remote_addr_log = remote_addr.to_string();
    let bind_addr = if remote_addr.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    vlog!(2, "[etrs] UDP forward → {remote_addr}");

    // Each local sender (peer_addr:peer_port) gets its own ephemeral UDP socket so
    // that remote replies arrive on a distinct source port and can be routed back to
    // the correct sender without any last-sender state.
    const SENDER_IDLE: Duration = Duration::from_secs(30);

    // Channel: per-sender reply tasks write here; one collector task drains to quic_send
    // (avoids concurrent writes to the single SendStream).
    let (reply_tx, mut reply_rx) = mpsc::channel::<Envelope>(256);

    let mut reply_task = tokio::spawn(async move {
        while let Some(env) = reply_rx.recv().await {
            if quic::write_msg(&mut quic_send, &env).await.is_err() {
                break;
            }
        }
    });

    // QUIC → remote UDP, with per-sender socket demux.
    let mut send_task = tokio::spawn(async move {
        // Local map — only this task accesses it, so no locking needed.
        let mut sender_map: HashMap<(String, u32), (Arc<UdpSocket>, Instant)> = HashMap::new();

        while let Ok(Some(env)) = quic::read_msg(&mut quic_recv).await {
            if let Some(Payload::UdpDatagram(dg)) = env.payload {
                if dg.peer_addr.is_empty() {
                    continue;
                }
                let key = (dg.peer_addr.clone(), dg.peer_port);
                let now = Instant::now();

                // Evict sockets idle longer than SENDER_IDLE.
                sender_map.retain(|_, (_, last)| now.duration_since(*last) < SENDER_IDLE);

                let socket = if let Some((sock, last)) = sender_map.get_mut(&key) {
                    *last = now;
                    Arc::clone(sock)
                } else {
                    // New sender: bind a fresh ephemeral socket.
                    let sock = match UdpSocket::bind(bind_addr).await {
                        Ok(s) => Arc::new(s),
                        Err(e) => {
                            vlog!(
                                1,
                                "[etrs] UDP forward: bind failed for sender {}: {e}",
                                key.0
                            );
                            continue;
                        }
                    };
                    sender_map.insert(key.clone(), (Arc::clone(&sock), now));

                    // Spawn a reply task: reads remote replies and routes them back to
                    // this specific sender via the shared channel.
                    let peer_addr = key.0.clone();
                    let peer_port = key.1;
                    let sock_r = Arc::clone(&sock);
                    let tx = reply_tx.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 65535];
                        while let Ok(Ok((n, _))) =
                            tokio::time::timeout(SENDER_IDLE, sock_r.recv_from(&mut buf)).await
                        {
                            let env = Envelope {
                                payload: Some(Payload::UdpDatagram(UdpDatagram {
                                    peer_addr: peer_addr.clone(),
                                    peer_port,
                                    data: buf[..n].to_vec(),
                                })),
                            };
                            if tx.send(env).await.is_err() {
                                break;
                            }
                        }
                    });

                    sock
                };

                let _ = socket.send_to(&dg.data, &remote_addr).await;
            }
        }
    });

    tokio::select! {
        _ = &mut reply_task => {}
        _ = &mut send_task  => {}
    }
    reply_task.abort();
    send_task.abort();
    vlog!(2, "[etrs] UDP forward to {remote_addr_log} closed");
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn find_free_display() -> Option<u16> {
    for d in 10..100 {
        let socket_path = format!("/tmp/.X11-unix/X{d}");
        if std::path::Path::new(&socket_path).exists() {
            continue;
        }
        let target_v4 = format!("127.0.0.1:{}", 6000 + d);
        let target_v6 = format!("[::1]:{}", 6000 + d);
        if std::net::TcpListener::bind(&target_v4).is_ok()
            && std::net::TcpListener::bind(&target_v6).is_ok()
        {
            return Some(d);
        }
    }
    None
}

struct X11Cleanup {
    display_num: u16,
}

impl Drop for X11Cleanup {
    fn drop(&mut self) {
        let d = self.display_num;
        let socket_path = format!("/tmp/.X11-unix/X{d}");
        std::fs::remove_file(&socket_path).ok();
        let _ = std::process::Command::new("xauth")
            .arg("remove")
            .arg(format!("localhost/unix:{d}"))
            .status();
        let _ = std::process::Command::new("xauth")
            .arg("remove")
            .arg(format!("localhost:{d}"))
            .status();
        vlog!(1, "[etrs] X11: cleaned up display {d}");
    }
}

async fn process_x11_setup<S>(
    mut stream: S,
    fake_cookie: &[u8],
    real_cookie: &[u8],
) -> io::Result<(S, Vec<u8>)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut header = [0u8; 12];
    stream.read_exact(&mut header).await?;

    let byte_order = header[0];
    let is_little = byte_order == 0x6c;

    let name_len = if is_little {
        u16::from_le_bytes([header[6], header[7]])
    } else {
        u16::from_be_bytes([header[6], header[7]])
    } as usize;

    let data_len = if is_little {
        u16::from_le_bytes([header[8], header[9]])
    } else {
        u16::from_be_bytes([header[8], header[9]])
    } as usize;

    let name_len_padded = (name_len + 3) & !3;
    let data_len_padded = (data_len + 3) & !3;

    let mut name_buf = vec![0u8; name_len_padded];
    stream.read_exact(&mut name_buf).await?;

    let mut data_buf = vec![0u8; data_len_padded];
    stream.read_exact(&mut data_buf).await?;

    if !fake_cookie.is_empty() {
        if data_len < fake_cookie.len() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "X11 auth cookie too short",
            ));
        }
        let client_cookie = &data_buf[..fake_cookie.len()];
        if client_cookie != fake_cookie {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "X11 auth cookie mismatch",
            ));
        }
    }

    let mut setup_block = Vec::new();
    if real_cookie.is_empty() {
        let mut new_header = header;
        new_header[6] = 0;
        new_header[7] = 0;
        new_header[8] = 0;
        new_header[9] = 0;
        setup_block.extend_from_slice(&new_header);
    } else {
        let proto_name = b"MIT-MAGIC-COOKIE-1";
        let name_len = proto_name.len();
        let name_len_padded = (name_len + 3) & !3;
        let mut new_name_buf = vec![0u8; name_len_padded];
        new_name_buf[..name_len].copy_from_slice(proto_name);

        let data_len = real_cookie.len();
        let data_len_padded = (data_len + 3) & !3;
        let mut new_data_buf = vec![0u8; data_len_padded];
        new_data_buf[..data_len].copy_from_slice(real_cookie);

        let mut new_header = header;
        if is_little {
            new_header[6..8].copy_from_slice(&(name_len as u16).to_le_bytes());
            new_header[8..10].copy_from_slice(&(data_len as u16).to_le_bytes());
        } else {
            new_header[6..8].copy_from_slice(&(name_len as u16).to_be_bytes());
            new_header[8..10].copy_from_slice(&(data_len as u16).to_be_bytes());
        }

        setup_block.extend_from_slice(&new_header);
        setup_block.extend_from_slice(&new_name_buf);
        setup_block.extend_from_slice(&new_data_buf);
    }

    Ok((stream, setup_block))
}

async fn forward_x11_connection<S>(stream: S, setup_block: Vec<u8>, conn: quinn::Connection)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut quic_send, quic_recv) = match conn.open_bi().await {
        Ok(s) => s,
        Err(_) => return,
    };
    if quic_send.write_all(&[TAG_FORWARD]).await.is_err() {
        return;
    }
    let env = Envelope {
        payload: Some(Payload::StreamOpen(StreamOpen {
            stream_id: 0,
            stream_type: etr::protocol::StreamType::X11 as i32,
            remote_host: String::new(),
            remote_port: 0,
            forward_proto: ForwardProto::Tcp as i32,
        })),
    };
    if quic::write_msg(&mut quic_send, &env).await.is_err() {
        return;
    }
    if quic_send.write_all(&setup_block).await.is_err() {
        return;
    }
    pipe_generic_quic(stream, quic_send, quic_recv).await;
}

async fn pipe_generic_quic<S>(
    stream: S,
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut r, mut w) = tokio::io::split(stream);

    let mut t1 = tokio::spawn(async move {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if quic_send.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = quic_send.finish();
    });

    let mut t2 = tokio::spawn(async move {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match quic_recv.read(&mut buf).await {
                Ok(None) | Err(_) => break,
                Ok(Some(0)) => continue,
                Ok(Some(n)) => {
                    if w.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = w.shutdown().await;
    });

    tokio::select! {
        _ = &mut t1 => {}
        _ = &mut t2 => {}
    }
    t1.abort();
    t2.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_defaults() {
        let cli = Cli::try_parse_from(["etrs"]).unwrap();
        assert_eq!(cli.port, 0);
        assert_eq!(cli.bind, "[::]");
        assert_eq!(cli.verbose, 0);
    }

    #[test]
    fn test_cli_verbose_count() {
        let cli = Cli::try_parse_from(["etrs", "-vvv"]).unwrap();
        assert_eq!(cli.verbose, 3);
    }

    #[test]
    fn test_cli_custom_port() {
        let cli = Cli::try_parse_from(["etrs", "--port", "3000"]).unwrap();
        assert_eq!(cli.port, 3000);
    }

    #[test]
    fn test_cli_help_valid() {
        let mut cmd = Cli::command();
        let help = cmd.render_help().to_string();
        assert!(help.contains("--port"));
        assert!(help.contains("--bind"));
    }

    #[test]
    fn test_hex_decode() {
        assert_eq!(hex_decode("deadbeef"), Some(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(hex_decode("odd"), None);
        assert_eq!(hex_decode("zz"), None);
    }

    #[test]
    fn test_cli_log_path() {
        let cli = Cli::try_parse_from(["etrs", "--log-path", "/tmp/server.log"]).unwrap();
        assert_eq!(
            cli.log_path,
            Some(std::path::PathBuf::from("/tmp/server.log"))
        );
    }

    #[test]
    fn test_completions_parsed() {
        for shell in ["bash", "zsh", "fish", "elvish", "power-shell", "nushell"] {
            let cli = Cli::try_parse_from(["etrs", "--completions", shell]).unwrap();
            assert!(cli.completions.is_some(), "expected Some for shell={shell}");
        }
    }

    #[test]
    fn test_completions_help_present() {
        let mut cmd = Cli::command();
        let help = cmd.render_help().to_string();
        assert!(help.contains("--completions"));
    }

    #[test]
    fn test_reconnect_timeout_flag() {
        let cli = Cli::try_parse_from(["etrs", "--reconnect-timeout", "3600"]).unwrap();
        assert_eq!(cli.reconnect_timeout, Some(3600));
    }

    #[test]
    fn test_reconnect_timeout_default_is_none() {
        let cli = Cli::try_parse_from(["etrs"]).unwrap();
        assert!(cli.reconnect_timeout.is_none());
    }

    #[test]
    fn test_reconnect_timeout_env_var() {
        // SAFETY: single-threaded test; no other threads read this var.
        unsafe { std::env::set_var("ETR_SERVER_NETWORK_TMOUT", "7200") };
        let timeout: u64 = std::env::var("ETR_SERVER_NETWORK_TMOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1800);
        assert_eq!(timeout, 7200);
        unsafe { std::env::remove_var("ETR_SERVER_NETWORK_TMOUT") };
    }

    #[test]
    fn test_reconnect_timeout_help_present() {
        let mut cmd = Cli::command();
        let help = cmd.render_help().to_string();
        assert!(help.contains("--reconnect-timeout"));
    }

    #[test]
    fn test_etrcmd_line_parsing() {
        // Simulate the bootstrap line-parsing logic for ETRCMD:.
        let lines = vec![
            "KEY=VALUE".to_string(),
            "ETRCMD:distrobox -- btop".to_string(),
            "OTHER=foo".to_string(),
        ];
        let mut extra_env: Vec<String> = Vec::new();
        let mut remote_command: Option<String> = None;
        for l in lines {
            if let Some(cmd) = l.strip_prefix("ETRCMD:") {
                remote_command = Some(cmd.to_string());
            } else {
                extra_env.push(l);
            }
        }
        assert_eq!(remote_command.as_deref(), Some("distrobox -- btop"));
        assert_eq!(extra_env, vec!["KEY=VALUE", "OTHER=foo"]);
    }

    #[test]
    fn test_etrcmd_absent_yields_none() {
        let lines: Vec<String> = vec!["KEY=VALUE".to_string()];
        let mut remote_command: Option<String> = None;
        for l in lines {
            if let Some(cmd) = l.strip_prefix("ETRCMD:") {
                remote_command = Some(cmd.to_string());
            }
        }
        assert!(remote_command.is_none());
    }

    #[test]
    fn test_etrx11_line_parsing() {
        let lines: Vec<String> = vec![
            "KEY=VALUE".to_string(),
            "ETRX11:true".to_string(),
            "OTHER=foo".to_string(),
        ];
        let mut x11_enabled = false;
        let mut extra_env = Vec::new();
        for l in lines {
            if l.starts_with("ETRX11:") {
                x11_enabled = true;
            } else {
                extra_env.push(l);
            }
        }
        assert!(x11_enabled);
        assert_eq!(extra_env, vec!["KEY=VALUE", "OTHER=foo"]);
    }

    #[tokio::test]
    async fn test_process_x11_setup_empty_real_cookie() {
        let mut input = vec![0x6cu8, 0, 11, 0, 0, 0];
        input.extend_from_slice(&(18u16.to_le_bytes()));
        input.extend_from_slice(&(16u16.to_le_bytes()));
        input.extend_from_slice(&[0, 0]);

        let proto_name = b"MIT-MAGIC-COOKIE-1";
        input.extend_from_slice(proto_name);
        input.extend_from_slice(&[0, 0]);

        let fake_cookie = [7u8; 16];
        input.extend_from_slice(&fake_cookie);

        let cursor = std::io::Cursor::new(input);
        let (_, setup_block) = process_x11_setup(cursor, &fake_cookie, &[]).await.unwrap();

        assert_eq!(setup_block.len(), 12);
        assert_eq!(setup_block[0], 0x6c);
        assert_eq!(setup_block[6], 0);
        assert_eq!(setup_block[7], 0);
        assert_eq!(setup_block[8], 0);
        assert_eq!(setup_block[9], 0);
    }

    #[tokio::test]
    async fn test_process_x11_setup_replace_cookie() {
        let mut input = vec![0x6cu8, 0, 11, 0, 0, 0];
        input.extend_from_slice(&(18u16.to_le_bytes()));
        input.extend_from_slice(&(16u16.to_le_bytes()));
        input.extend_from_slice(&[0, 0]);

        let proto_name = b"MIT-MAGIC-COOKIE-1";
        input.extend_from_slice(proto_name);
        input.extend_from_slice(&[0, 0]);

        let fake_cookie = [7u8; 16];
        input.extend_from_slice(&fake_cookie);

        let real_cookie = [9u8; 16];
        let cursor = std::io::Cursor::new(input);
        let (_, setup_block) = process_x11_setup(cursor, &fake_cookie, &real_cookie)
            .await
            .unwrap();

        assert_eq!(setup_block[0], 0x6c);
        let name_len = u16::from_le_bytes([setup_block[6], setup_block[7]]);
        assert_eq!(name_len, 18);
        let data_len = u16::from_le_bytes([setup_block[8], setup_block[9]]);
        assert_eq!(data_len, 16);
        assert_eq!(&setup_block[12..30], proto_name);
        assert_eq!(&setup_block[32..48], &real_cookie);
    }

    #[tokio::test]
    async fn test_process_x11_setup_skip_validation() {
        let mut input = vec![0x6cu8, 0, 11, 0, 0, 0];
        input.extend_from_slice(&(0u16.to_le_bytes()));
        input.extend_from_slice(&(0u16.to_le_bytes()));
        input.extend_from_slice(&[0, 0]);

        let cursor = std::io::Cursor::new(input);
        let (_, setup_block) = process_x11_setup(cursor, &[], &[]).await.unwrap();
        assert_eq!(setup_block.len(), 12);
    }

    #[tokio::test]
    async fn test_process_x11_setup_mismatch() {
        let mut input = vec![0x6cu8, 0, 11, 0, 0, 0];
        input.extend_from_slice(&(18u16.to_le_bytes()));
        input.extend_from_slice(&(16u16.to_le_bytes()));
        input.extend_from_slice(&[0, 0]);

        let proto_name = b"MIT-MAGIC-COOKIE-1";
        input.extend_from_slice(proto_name);
        input.extend_from_slice(&[0, 0]);

        let fake_cookie = [7u8; 16];
        input.extend_from_slice(&[8u8; 16]);

        let cursor = std::io::Cursor::new(input);
        let res = process_x11_setup(cursor, &fake_cookie, &[]).await;
        assert!(res.is_err());
    }
}
