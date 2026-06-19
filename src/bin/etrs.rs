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
            run_session(endpoint, session_id, passkey, term).await
        })
}

fn detach_stdio(log_path: &std::path::Path) -> io::Result<()> {
    use nix::unistd::dup2;
    use std::os::unix::io::IntoRawFd;

    let null_fd = std::fs::File::open("/dev/null")
        .map(|f| f.into_raw_fd())
        .unwrap_or(-1);

    if let Some(p) = log_path.parent() {
        std::fs::create_dir_all(p).ok();
    }
    let log_fd = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map(|f| f.into_raw_fd())
        .unwrap_or(null_fd);

    if null_fd >= 0 {
        dup2(null_fd, 0).ok();
        dup2(null_fd, 1).ok();
    }
    if log_fd >= 0 {
        dup2(log_fd, 2).ok();
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
/// the 30-minute reconnect window expires.
async fn run_session(
    endpoint: quinn::Endpoint,
    session_id: [u8; 16],
    passkey: String,
    term: String,
) -> io::Result<()> {
    vlog!(
        1,
        "[etrs] session {} port={}",
        hex_encode(&session_id),
        endpoint.local_addr()?.port()
    );

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

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let mut cmd = CommandBuilder::new(&shell);
    cmd.env("TERM", &term);
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

    let active_conn = Arc::new(Mutex::new(None));
    let active_reverse_listeners = Arc::new(Mutex::new(std::collections::HashSet::new()));

    // ── Reconnect loop ───────────────────────────────────────────────────────
    const RECONNECT_WINDOW: Duration = Duration::from_secs(30 * 60);

    loop {
        let mut shell_rx = shell_exit_rx.clone();
        let incoming = tokio::select! {
            biased;
            _ = shell_rx.wait_for(|&v| v) => {
                vlog!(1, "[etrs] shell exited, shutting down");
                break;
            }
            res = tokio::time::timeout(RECONNECT_WINDOW, endpoint.accept()) => {
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

        let clean = handle_connection(
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
        )
        .await;

        *outbound_pty_tx.lock().unwrap() = None;
        *outbound_ctrl_tx.lock().unwrap() = None;
        *active_conn.lock().await = None;

        if clean {
            vlog!(1, "[etrs] client disconnected cleanly, shutting down");
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
) -> bool {
    let peer = conn.remote_address();
    vlog!(
        1,
        "[etrs] connection from {} session={}",
        peer,
        hex_encode(&session_id)
    );
    if let Some(fd) = master_fd {
        let addr = format!("{} via etr [{}]", peer.ip(), std::process::id());
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
/// from the client back to the last-seen sender (last-sender routing).
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
    let last_peer: Arc<std::sync::Mutex<Option<std::net::SocketAddr>>> =
        Arc::new(std::sync::Mutex::new(None));

    let socket_recv = Arc::clone(&socket);
    let last_peer_recv = Arc::clone(&last_peer);
    let active_conn_recv = Arc::clone(&active_conn);
    let current_quic_tx_recv = Arc::clone(&current_quic_tx);
    let last_conn_id_recv = Arc::clone(&last_conn_id);
    let spec_recv = spec.clone();

    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        while let Ok((n, src)) = socket_recv.recv_from(&mut buf).await {
            vlog!(3, "[etrs] UDP reverse fwd received {n} bytes from {src}");
            *last_peer_recv.lock().unwrap() = Some(src);

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
                    let last_peer_send = Arc::clone(&last_peer_recv);
                    let mut rx = rx;
                    tokio::spawn(async move {
                        vlog!(3, "[etrs] UDP reverse fwd: rx reader task started");
                        while let Ok(Some(env)) = quic::read_msg(&mut rx).await {
                            if let Some(Payload::UdpDatagram(dg)) = env.payload {
                                let target_addr = {
                                    let g = last_peer_send.lock().unwrap();
                                    *g
                                };
                                if let Some(addr) = target_addr {
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
    use tokio::net::UdpSocket;

    // Resolve the remote host to a concrete SocketAddr so that:
    // (a) the UDP socket is bound to the matching IP family, and
    // (b) send_to uses the already-resolved address instead of re-resolving
    //     (which on macOS returns ::1 first, causing IPv4-socket sends to fail).
    let remote_addr_str = format!("{}:{}", so.remote_host, so.remote_port);
    // Prefer IPv4 when the hostname resolves to both families (e.g. "localhost"
    // on macOS returns ::1 first).  Fall back to whatever the resolver gives us.
    let remote_addr: std::net::SocketAddr = match tokio::net::lookup_host(&remote_addr_str)
        .await
        .ok()
        .and_then(|it| {
            let addrs: Vec<std::net::SocketAddr> = it.collect();
            addrs
                .iter()
                .find(|a| a.is_ipv4())
                .copied()
                .or_else(|| addrs.into_iter().next())
        }) {
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
    let socket = match UdpSocket::bind(bind_addr).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            vlog!(1, "[etrs] UDP bind failed: {e}");
            let _ = quic_send.finish();
            return;
        }
    };
    vlog!(2, "[etrs] UDP forward → {remote_addr}");

    let last_peer: Arc<std::sync::Mutex<Option<(String, u32)>>> =
        Arc::new(std::sync::Mutex::new(None));
    let last_peer2 = Arc::clone(&last_peer);
    let socket2 = Arc::clone(&socket);

    // Remote replies → QUIC (as UdpDatagram envelopes).
    let mut reply_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        while let Ok((n, _src)) = socket2.recv_from(&mut buf).await {
            let (peer_addr, peer_port) = {
                let g = last_peer2.lock().unwrap();
                match g.as_ref() {
                    Some((a, p)) => (a.clone(), *p),
                    None => continue,
                }
            };
            let env = Envelope {
                payload: Some(Payload::UdpDatagram(UdpDatagram {
                    peer_addr,
                    peer_port,
                    data: buf[..n].to_vec(),
                })),
            };
            if quic::write_msg(&mut quic_send, &env).await.is_err() {
                break;
            }
        }
    });

    // QUIC (UdpDatagram envelopes) → remote UDP.
    let mut send_task = tokio::spawn(async move {
        while let Ok(Some(env)) = quic::read_msg(&mut quic_recv).await {
            if let Some(Payload::UdpDatagram(dg)) = env.payload {
                if !dg.peer_addr.is_empty() {
                    *last_peer.lock().unwrap() = Some((dg.peer_addr.clone(), dg.peer_port));
                }
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
}
