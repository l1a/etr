// SPDX-License-Identifier: GPL-3.0-or-later
use clap::{ArgAction, CommandFactory, Parser, ValueEnum};
use clap_complete::Shell;
use clap_complete_nushell::Nushell;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, IsTerminal, Write};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

use etr::config::Config;
use etr::forward::ForwardSpec;
use etr::protocol::{
    Envelope, ForwardProto, Heartbeat, Payload, SessionOpen, StreamOpen, TerminalResize,
    UdpDatagram,
};
use etr::quic::{self, TAG_CONTROL, TAG_FORWARD, TAG_PTY};
use etr::session::SessionState;

static LOG_FILE: std::sync::OnceLock<std::sync::Mutex<std::fs::File>> = std::sync::OnceLock::new();
static IN_RAW_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

macro_rules! vlog {
    ($verbose:expr, $level:expr, $($arg:tt)*) => {
        if $verbose >= $level {
            let raw = IN_RAW_MODE.load(std::sync::atomic::Ordering::Relaxed);
            if !raw {
                eprintln!($($arg)*);
            }
            if let Some(f) = LOG_FILE.get() {
                let _ = writeln!(f.lock().unwrap(), $($arg)*);
            }
        }
    };
}

fn client_log_path() -> std::path::PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                .join(".local/state")
        })
        .join("etr")
        .join("etr.log")
}

#[derive(Parser, Debug)]
#[command(
    name = "etr",
    version = "0.2.0",
    about = "Eternal Terminal Client in Rust"
)]
struct Cli {
    /// Remote host (e.g. user@host or host)
    target: Option<String>,

    /// SSH port for initial authentication (default: 22, or config file ssh_port)
    #[arg(short = 's', long)]
    ssh_port: Option<u16>,

    /// Verbosity: -v connection events, -vv QUIC details, -vvv stream trace
    #[arg(short = 'v', action = ArgAction::Count)]
    verbose: u8,

    /// Path to the etrs binary on the remote host (default: relies on PATH)
    #[arg(long)]
    server_path: Option<String>,

    /// Local port forwarding (repeatable): local_port:remote_host:remote_port[/tcp|/udp]
    /// Works like ssh -L. Default protocol: tcp.
    /// Example: -L 8080:localhost:80  -L 5353:8.8.8.8:53/udp
    #[arg(short = 'L', value_name = "SPEC")]
    forward: Vec<String>,

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

#[tokio::main]
async fn main() -> io::Result<()> {
    let cli = Cli::parse();

    if let Some(shell) = cli.completions {
        let mut cmd = Cli::command();
        match shell {
            ShellChoice::Bash => {
                clap_complete::generate(Shell::Bash, &mut cmd, "etr", &mut io::stdout())
            }
            ShellChoice::Elvish => {
                clap_complete::generate(Shell::Elvish, &mut cmd, "etr", &mut io::stdout())
            }
            ShellChoice::Fish => {
                clap_complete::generate(Shell::Fish, &mut cmd, "etr", &mut io::stdout())
            }
            ShellChoice::PowerShell => {
                clap_complete::generate(Shell::PowerShell, &mut cmd, "etr", &mut io::stdout())
            }
            ShellChoice::Zsh => {
                clap_complete::generate(Shell::Zsh, &mut cmd, "etr", &mut io::stdout())
            }
            ShellChoice::Nushell => {
                clap_complete::generate(Nushell, &mut cmd, "etr", &mut io::stdout())
            }
        }
        return Ok(());
    }

    if cli.verbose > 0 && io::stdin().is_terminal() {
        let log_path = client_log_path();
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(f) => {
                eprintln!("[etr] Verbose log → {}", log_path.display());
                let _ = LOG_FILE.set(std::sync::Mutex::new(f));
            }
            Err(e) => eprintln!("[etr] Could not open log file: {e}"),
        }
    }

    let target = match cli.target {
        Some(t) => t,
        None => {
            let _ = Cli::command().print_help();
            return Ok(());
        }
    };

    let cfg = Config::load();
    let ssh_port = cli
        .ssh_port
        .unwrap_or_else(|| cfg.client.ssh_port.unwrap_or(22));
    let server_path = cli
        .server_path
        .or(cfg.client.server_path)
        .unwrap_or_else(|| "etrs".to_string());

    let mut forward_specs: Vec<ForwardSpec> = Vec::new();
    for s in &cli.forward {
        match ForwardSpec::parse(s) {
            Ok(spec) => {
                vlog!(cli.verbose, 1, "[etr] Forwarding: {spec}");
                forward_specs.push(spec);
            }
            Err(e) => {
                eprintln!("[etr] error: {e}");
                return Ok(());
            }
        }
    }

    let session_id = generate_session_id();
    let passkey = generate_passkey();
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());

    vlog!(
        cli.verbose,
        1,
        "[etr] Connecting to {} via SSH to bootstrap session...",
        target
    );

    let (server_port, server_cert) = bootstrap_ssh(
        &target,
        ssh_port,
        &session_id,
        &passkey,
        &term,
        &server_path,
        cli.verbose,
    )?;

    vlog!(cli.verbose, 2, "[etr] etrs bound to port {server_port}");

    let session = Arc::new(Mutex::new(SessionState::new(session_id, passkey.clone())));

    if let Err(e) = run_connection_loop(
        target,
        server_port,
        server_cert,
        passkey,
        session_id,
        session,
        forward_specs,
        cli.verbose,
    )
    .await
    {
        eprintln!("Session terminated: {:?}", e);
    }

    Ok(())
}

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

fn generate_session_id() -> [u8; 16] {
    use rand::Rng;
    rand::thread_rng().r#gen()
}

fn generate_passkey() -> String {
    use rand::Rng;
    rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

/// SSH to the target, start `etrs`, send session credentials, and read back
/// the QUIC port and server cert DER from etrs stdout.
fn bootstrap_ssh(
    target: &str,
    ssh_port: u16,
    session_id: &[u8; 16],
    passkey: &str,
    term: &str,
    server_path: &str,
    verbose: u8,
) -> io::Result<(u16, Vec<u8>)> {
    let session_id_hex = hex_encode(session_id);
    let v_flag = match verbose {
        0 => String::new(),
        n => format!("-{}", "v".repeat(n as usize)),
    };
    let mut cmd = Command::new("ssh");
    cmd.arg("-p").arg(ssh_port.to_string()).arg(target);
    cmd.arg(server_path);
    if !v_flag.is_empty() {
        cmd.arg(&v_flag);
    }
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("Failed to open SSH stdin pipe"))?;
    stdin.write_all(format!("{}/{}/{}\n", session_id_hex, passkey, term).as_bytes())?;
    stdin.flush()?;
    drop(stdin);

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "SSH bootstrap failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    // Parse "PORT <n> CERT <cert_hex>" from etrs stdout.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("PORT ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 3
                && parts[1] == "CERT"
                && let Ok(port) = parts[0].parse::<u16>()
                && let Some(cert) = hex_decode(parts[2])
            {
                return Ok((port, cert));
            }
        }
    }
    Err(io::Error::other(format!(
        "etrs did not report PORT/CERT (stdout: {:?})",
        stdout.trim()
    )))
}

#[allow(clippy::too_many_arguments)]
async fn run_connection_loop(
    target: String,
    port: u16,
    server_cert: Vec<u8>,
    passkey: String,
    session_id: [u8; 16],
    session: Arc<Mutex<SessionState>>,
    forward_specs: Vec<ForwardSpec>,
    verbose: u8,
) -> io::Result<()> {
    let host = if let Some(idx) = target.find('@') {
        &target[idx + 1..]
    } else {
        &target
    };
    let server_addr = tokio::net::lookup_host(format!("{host}:{port}"))
        .await?
        .next()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("could not resolve host: {host}"),
            )
        })?;

    // Build QUIC client endpoint + config (reused across reconnects).
    let cert = rustls::pki_types::CertificateDer::from(server_cert);
    let cli_cfg = quic::client_config(cert)?;
    let bind_addr = if server_addr.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let mut endpoint = quinn::Endpoint::client(bind_addr.parse().unwrap())
        .map_err(io::Error::other)?;
    endpoint.set_default_client_config(cli_cfg);

    // Single stdin reader shared across all reconnect iterations.
    let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(1000);
    let stdin_rx = Arc::new(Mutex::new(stdin_rx));

    let _stdin_reader = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut buf = [0u8; 1024];
        while let Ok(n) = std::io::stdin().read(&mut buf) {
            if n == 0 {
                break;
            }
            if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut first = true;
    loop {
        if !first {
            vlog!(verbose, 1, "\r\n[etr] Reconnecting to {server_addr}...");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        first = false;

        vlog!(
            verbose,
            2,
            "[etr] Connecting  session={}",
            hex_encode(&session_id)
        );

        let conn = match endpoint.connect(server_addr, "etr") {
            Ok(c) => c,
            Err(e) => {
                vlog!(verbose, 1, "[etr] Connect error: {e}");
                continue;
            }
        };
        let conn = match conn.await {
            Ok(c) => c,
            Err(e) => {
                vlog!(verbose, 1, "[etr] QUIC handshake failed: {e}");
                continue;
            }
        };

        vlog!(verbose, 2, "[etr] QUIC connected to {server_addr}");

        enable_raw_mode().unwrap();
        IN_RAW_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
        let result = run_session(
            conn,
            session_id,
            passkey.clone(),
            Arc::clone(&session),
            Arc::clone(&stdin_rx),
            forward_specs.clone(),
            verbose,
        )
        .await;
        IN_RAW_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
        let _ = disable_raw_mode();

        match result {
            Ok(_) => {
                vlog!(verbose, 1, "[etr] Connection closed cleanly.");
                std::process::exit(0);
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {
                vlog!(verbose, 1, "[etr] Connection closed cleanly.");
                std::process::exit(0);
            }
            Err(e) => {
                vlog!(verbose, 1, "[etr] Session dropped: {e:?}");
            }
        }
    }
}

async fn run_session(
    conn: quinn::Connection,
    session_id: [u8; 16],
    passkey: String,
    session: Arc<Mutex<SessionState>>,
    stdin_rx: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
    forward_specs: Vec<ForwardSpec>,
    verbose: u8,
) -> io::Result<()> {
    // ── Open control stream ───────────────────────────────────────────────
    let (mut ctrl_send, mut ctrl_recv) = conn.open_bi().await.map_err(io::Error::other)?;
    ctrl_send
        .write_all(&[TAG_CONTROL])
        .await
        .map_err(io::Error::other)?;

    let last_received = {
        let s = session.lock().await;
        s.last_received_map()
    };

    let session_open = Envelope {
        payload: Some(Payload::SessionOpen(SessionOpen {
            session_id: session_id.to_vec(),
            passkey: passkey.clone(),
            last_received_seq: last_received,
        })),
    };
    quic::write_msg(&mut ctrl_send, &session_open).await?;

    // Read SessionAccept.
    let server_acks = match quic::read_msg(&mut ctrl_recv).await? {
        Some(env) => match env.payload {
            Some(Payload::SessionAccept(sa)) => sa.last_received_seq,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expected SessionAccept",
                ));
            }
        },
        None => {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "control stream closed before SessionAccept",
            ));
        }
    };

    vlog!(verbose, 1, "\r\n[etr] Connected. Session active.");

    // Trim send history using server's ack map.
    {
        let mut s = session.lock().await;
        s.apply_server_acks(&server_acks);
    }

    // ── Open PTY stream ───────────────────────────────────────────────────
    let (mut pty_send, mut pty_recv) = conn.open_bi().await.map_err(io::Error::other)?;
    pty_send
        .write_all(&[TAG_PTY])
        .await
        .map_err(io::Error::other)?;

    // Replay any unacknowledged stdin the server hasn't seen.
    {
        let s = session.lock().await;
        let replays = s.collect_replays(&server_acks);
        if let Some(stream0_replays) = replays.get(&0) {
            for (seq, data) in stream0_replays {
                quic::write_pty_chunk(&mut pty_send, *seq, data).await?;
            }
        }
    }

    // ── stdout writer task ────────────────────────────────────────────────
    let (stdout_tx, mut stdout_rx) = mpsc::channel::<Vec<u8>>(512);
    let mut stdout_task = tokio::spawn(async move {
        use std::io::BufWriter;
        let mut out = BufWriter::with_capacity(256 * 1024, io::stdout());
        while let Some(data) = stdout_rx.recv().await {
            let _ = out.write_all(&data);
            while let Ok(more) = stdout_rx.try_recv() {
                let _ = out.write_all(&more);
            }
            let _ = out.flush();
        }
    });

    // ── PTY recv task: QUIC PTY recv → stdout ────────────────────────────
    let session_r = Arc::clone(&session);
    let stdout_tx2 = stdout_tx.clone();
    let mut pty_recv_task = tokio::spawn(async move {
        loop {
            match quic::read_pty_chunk(&mut pty_recv).await {
                Ok(Some((seq, data))) => {
                    {
                        let mut s = session_r.lock().await;
                        if let Some(st) = s.stream_mut(0) {
                            st.next_in_seq = seq + 1;
                        }
                    }
                    let _ = stdout_tx2.try_send(data);
                }
                Ok(None) => break,
                Err(_) => {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "PTY stream closed"));
                }
            }
        }
        Ok(())
    });

    // ── stdin task: stdin → QUIC PTY send ────────────────────────────────
    let session_stdin = Arc::clone(&session);
    let mut stdin_task = tokio::spawn(async move {
        loop {
            let payload = {
                let mut rx = stdin_rx.lock().await;
                match rx.recv().await {
                    Some(p) => p,
                    None => break,
                }
            };
            let seq = {
                let mut s = session_stdin.lock().await;
                let st = s.stream_mut(0).expect("stream 0 always exists");
                let seq = st.next_out_seq;
                st.next_out_seq += 1;
                st.record_send(seq, payload.clone());
                seq
            };
            if quic::write_pty_chunk(&mut pty_send, seq, &payload)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // ── Control reader task: ctrl_recv → dispatch ─────────────────────────
    let mut ctrl_reader_task: tokio::task::JoinHandle<io::Result<()>> =
        tokio::spawn(async move {
            loop {
                match quic::read_msg(&mut ctrl_recv).await {
                    Ok(Some(env)) => match env.payload {
                        Some(Payload::Disconnect(_)) => {
                            return Err(io::Error::new(
                                io::ErrorKind::ConnectionAborted,
                                "clean disconnect from server",
                            ));
                        }
                        Some(Payload::Heartbeat(_)) => {}
                        _ => {}
                    },
                    Ok(None) => break,
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        });

    // ── Resize task: SIGWINCH → ctrl_send ────────────────────────────────
    // Use a channel so ctrl_send isn't shared across tasks.
    let (resize_tx, mut resize_rx) = mpsc::channel::<TerminalResize>(4);

    let mut sigwinch_task = tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut sigwinch) = signal(SignalKind::window_change()) {
            while sigwinch.recv().await.is_some() {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    let _ = resize_tx.try_send(TerminalResize {
                        rows: rows as u32,
                        cols: cols as u32,
                    });
                }
            }
        }
    });

    // ── Heartbeat + resize writer: ctrl_send ──────────────────────────────
    let mut ctrl_send_task: tokio::task::JoinHandle<io::Result<()>> =
        tokio::spawn(async move {
            // Send initial terminal size.
            if let Ok((cols, rows)) = crossterm::terminal::size() {
                let env = Envelope {
                    payload: Some(Payload::TerminalResize(TerminalResize {
                        rows: rows as u32,
                        cols: cols as u32,
                    })),
                };
                quic::write_msg(&mut ctrl_send, &env).await?;
            }

            let mut hb_interval = tokio::time::interval(Duration::from_secs(5));
            hb_interval.tick().await; // skip the immediate first tick
            loop {
                tokio::select! {
                    _ = hb_interval.tick() => {
                        let env = Envelope { payload: Some(Payload::Heartbeat(Heartbeat {})) };
                        quic::write_msg(&mut ctrl_send, &env).await?;
                    }
                    Some(tr) = resize_rx.recv() => {
                        let env = Envelope { payload: Some(Payload::TerminalResize(tr)) };
                        quic::write_msg(&mut ctrl_send, &env).await?;
                    }
                }
            }
        });

    // ── Forward tasks ─────────────────────────────────────────────────────
    let mut fwd_handles = Vec::new();
    for spec in &forward_specs {
        let conn2 = conn.clone();
        let spec2 = spec.clone();
        let handle = match spec.proto {
            ForwardProto::Tcp => {
                tokio::spawn(run_tcp_acceptor_quic(spec2, conn2, verbose))
            }
            ForwardProto::Udp => {
                tokio::spawn(run_udp_forward_client_quic(spec2, conn2, verbose))
            }
        };
        fwd_handles.push(handle);
    }

    // ── Wait for any task to complete ─────────────────────────────────────
    let result = tokio::select! {
        r = &mut ctrl_reader_task => r.unwrap_or_else(|e| Err(io::Error::other(e.to_string()))),
        _ = &mut stdin_task        => Ok(()),
        r = &mut pty_recv_task     => r.unwrap_or_else(|e| Err(io::Error::other(e.to_string()))),
        _ = &mut ctrl_send_task    => Ok(()),
        _ = &mut sigwinch_task     => Ok(()),
        _ = &mut stdout_task       => Ok(()),
    };

    ctrl_reader_task.abort();
    stdin_task.abort();
    pty_recv_task.abort();
    ctrl_send_task.abort();
    sigwinch_task.abort();
    stdout_task.abort();
    for h in fwd_handles {
        h.abort();
    }

    result
}

// ── Forward helpers (client side) ────────────────────────────────────────────

/// Accept local TCP connections and open a QUIC forward stream per connection.
async fn run_tcp_acceptor_quic(spec: ForwardSpec, conn: quinn::Connection, verbose: u8) {
    use tokio::net::TcpListener;

    let listener = match TcpListener::bind(format!("127.0.0.1:{}", spec.local_port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[etr] cannot bind TCP port {}: {e}", spec.local_port);
            return;
        }
    };
    vlog!(
        verbose,
        1,
        "[etr] TCP forward  local:{} → {}:{}",
        spec.local_port,
        spec.remote_host,
        spec.remote_port
    );

    loop {
        let (tcp_stream, peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                vlog!(verbose, 1, "[etr] TCP accept error: {e}");
                break;
            }
        };
        vlog!(verbose, 2, "[etr] TCP connect from {peer}");

        let conn2 = conn.clone();
        let spec2 = spec.clone();
        tokio::spawn(async move {
            let (mut quic_send, quic_recv) = match conn2.open_bi().await {
                Ok(s) => s,
                Err(_) => return,
            };
            let so = Envelope {
                payload: Some(Payload::StreamOpen(StreamOpen {
                    stream_id: 0,
                    stream_type: etr::protocol::StreamType::PortForward as i32,
                    remote_host: spec2.remote_host.clone(),
                    remote_port: spec2.remote_port as u32,
                    forward_proto: ForwardProto::Tcp as i32,
                })),
            };
            if quic_send.write_all(&[TAG_FORWARD]).await.is_err() {
                return;
            }
            if quic::write_msg(&mut quic_send, &so).await.is_err() {
                return;
            }
            run_tcp_connection_quic(tcp_stream, quic_send, quic_recv).await;
        });
    }
}

/// Pipe one TCP connection ↔ one QUIC forward stream.
async fn run_tcp_connection_quic(
    stream: tokio::net::TcpStream,
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut tcp_r, mut tcp_w) = stream.into_split();

    let mut t1 = tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
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
        let mut buf = vec![0u8; 8192];
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

/// Open one QUIC forward stream for a UDP `-L` spec; pipe local datagrams through it.
async fn run_udp_forward_client_quic(spec: ForwardSpec, conn: quinn::Connection, verbose: u8) {
    use tokio::net::UdpSocket;

    let local_socket = match UdpSocket::bind(format!("0.0.0.0:{}", spec.local_port)).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("[etr] cannot bind UDP port {}: {e}", spec.local_port);
            return;
        }
    };
    vlog!(
        verbose,
        1,
        "[etr] UDP forward  local:{} → {}:{}",
        spec.local_port,
        spec.remote_host,
        spec.remote_port
    );

    let (mut quic_send, mut quic_recv) = match conn.open_bi().await {
        Ok(s) => s,
        Err(e) => {
            vlog!(verbose, 1, "[etr] open_bi error for UDP forward: {e}");
            return;
        }
    };

    let so = Envelope {
        payload: Some(Payload::StreamOpen(StreamOpen {
            stream_id: 0,
            stream_type: etr::protocol::StreamType::PortForward as i32,
            remote_host: spec.remote_host.clone(),
            remote_port: spec.remote_port as u32,
            forward_proto: ForwardProto::Udp as i32,
        })),
    };
    if quic_send.write_all(&[TAG_FORWARD]).await.is_err() {
        return;
    }
    if quic::write_msg(&mut quic_send, &so).await.is_err() {
        return;
    }

    let local_socket2 = Arc::clone(&local_socket);

    // Local UDP datagrams → QUIC (as UdpDatagram envelopes).
    let mut dgram_in = tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        while let Ok((n, src)) = local_socket2.recv_from(&mut buf).await {
            let env = Envelope {
                payload: Some(Payload::UdpDatagram(UdpDatagram {
                    peer_addr: src.ip().to_string(),
                    peer_port: src.port() as u32,
                    data: buf[..n].to_vec(),
                })),
            };
            if quic::write_msg(&mut quic_send, &env).await.is_err() {
                break;
            }
        }
    });

    // QUIC (UdpDatagram envelopes) → local UDP senders.
    let mut dgram_out = tokio::spawn(async move {
        while let Ok(Some(env)) = quic::read_msg(&mut quic_recv).await {
            if let Some(Payload::UdpDatagram(dg)) = env.payload
                && !dg.peer_addr.is_empty()
                && dg.peer_port > 0
            {
                let dest = format!("{}:{}", dg.peer_addr, dg.peer_port);
                let _ = local_socket.send_to(&dg.data, &dest).await;
            }
        }
    });

    tokio::select! {
        _ = &mut dgram_in  => {}
        _ = &mut dgram_out => {}
    }
    dgram_in.abort();
    dgram_out.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_verbose_count() {
        let cli = Cli::try_parse_from(["etr", "-vvv", "host"]).unwrap();
        assert_eq!(cli.verbose, 3);
    }

    #[test]
    fn test_verbose_default() {
        let cli = Cli::try_parse_from(["etr", "host"]).unwrap();
        assert_eq!(cli.verbose, 0);
    }

    #[test]
    fn test_help_valid() {
        let mut cmd = Cli::command();
        let help = cmd.render_help().to_string();
        assert!(help.contains("Verbosity") || help.contains("-v"));
    }

    #[test]
    fn test_target_passthrough() {
        let cli = Cli::try_parse_from(["etr", "user@host"]).unwrap();
        assert_eq!(cli.target.as_deref(), Some("user@host"));
        let cli = Cli::try_parse_from(["etr", "localhost"]).unwrap();
        assert_eq!(cli.target.as_deref(), Some("localhost"));
    }

    #[test]
    fn test_ssh_port_default_is_none() {
        let cli = Cli::try_parse_from(["etr", "host"]).unwrap();
        assert_eq!(cli.ssh_port, None);
    }

    #[test]
    fn test_ssh_port_override() {
        let cli = Cli::try_parse_from(["etr", "-s", "2222", "host"]).unwrap();
        assert_eq!(cli.ssh_port, Some(2222));
    }

    #[test]
    fn test_no_cipher_flag() {
        // --cipher is removed; the parser should have no such argument.
        let result = Cli::try_parse_from(["etr", "--cipher", "x25519-aes", "host"]);
        assert!(result.is_err());
    }
}
