// SPDX-License-Identifier: GPL-3.0-or-later
use clap::{ArgAction, CommandFactory, Parser, ValueEnum};
use clap_complete::Shell;
use clap_complete_nushell::Nushell;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc};

use etr::crypto::{CipherSuiteId, generate_session_id};

/// Log file for verbose output — set once at startup when running interactively.
static LOG_FILE: std::sync::OnceLock<std::sync::Mutex<std::fs::File>> = std::sync::OnceLock::new();

/// Set to true when raw mode is active; suppresses stderr logging to avoid
/// corrupting the terminal display.
static IN_RAW_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Log at `$level`:
/// - before raw mode: stderr (always visible) + log file (if open)
/// - during raw mode: log file only (stderr would corrupt the display)
/// - no log file open and not in raw mode: stderr
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

fn payload_type(p: Option<&Payload>) -> &'static str {
    match p {
        Some(Payload::ClientHello(_)) => "ClientHello",
        Some(Payload::ServerHello(_)) => "ServerHello",
        Some(Payload::StreamOpen(_)) => "StreamOpen",
        Some(Payload::StreamClose(_)) => "StreamClose",
        Some(Payload::StreamData(_)) => "StreamData",
        Some(Payload::StreamAck(_)) => "StreamAck",
        Some(Payload::TerminalResize(_)) => "TerminalResize",
        Some(Payload::Heartbeat(_)) => "Heartbeat",
        Some(Payload::Disconnect(_)) => "Disconnect",
        None => "Empty",
    }
}
use etr::handshake::ClientHandshake;
use etr::protocol::{Envelope, Heartbeat, PacketHeader, Payload, StreamData, TerminalResize};
use etr::session::SessionState;
use etr::transport::{decode_data_packet, decode_plaintext_packet, recv_packet, send_packet};
use prost::Message as _;

#[derive(Parser, Debug)]
#[command(
    name = "etr",
    version = "0.2.0",
    about = "Eternal Terminal Client in Rust"
)]
struct Cli {
    /// Remote host (e.g. user@host or host)
    target: Option<String>,

    /// SSH port for initial authentication
    #[arg(short = 's', long, default_value = "22")]
    ssh_port: u16,

    /// Verbosity: -v connection events, -vv cipher details, -vvv packet trace
    #[arg(short = 'v', action = ArgAction::Count)]
    verbose: u8,

    /// Path to the etrs binary on the remote host (default: relies on PATH)
    #[arg(long, default_value = "etrs")]
    server_path: String,

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

    // In interactive mode, route verbose logs to a file so they don't corrupt
    // the raw-mode terminal display. Print the path once so the user knows
    // where to look.
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

    let target_input = match cli.target {
        Some(t) => t,
        None => {
            let _ = Cli::command().print_help();
            return Ok(());
        }
    };

    let target = target_input;

    let session_id = generate_session_id();
    let passkey = generate_passkey();
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());

    vlog!(
        cli.verbose,
        1,
        "[etr] Connecting to {} via SSH to bootstrap session...",
        target
    );

    let server_port = bootstrap_ssh(
        &target,
        cli.ssh_port,
        &session_id,
        &passkey,
        &term,
        &cli.server_path,
    )?;

    vlog!(cli.verbose, 2, "[etr] etrs bound to port {}", server_port);

    let session = Arc::new(Mutex::new(SessionState::new(session_id, passkey.clone())));

    if let Err(e) = run_connection_loop(
        target,
        server_port,
        passkey,
        session_id,
        session,
        cli.verbose,
    )
    .await
    {
        eprintln!("Session terminated: {:?}", e);
    }

    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn generate_passkey() -> String {
    use rand::Rng;
    rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

/// SSH to the target, start `etrs`, send session credentials via stdin,
/// and read back the UDP port that `etrs` bound.
fn bootstrap_ssh(
    target: &str,
    ssh_port: u16,
    session_id: &[u8; 16],
    passkey: &str,
    term: &str,
    server_path: &str,
) -> io::Result<u16> {
    let session_id_hex = hex_encode(session_id);
    let mut child = Command::new("ssh")
        .arg("-p")
        .arg(ssh_port.to_string())
        .arg(target)
        .arg(server_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("Failed to open SSH stdin pipe"))?;
    // Format: SESSION_ID_HEX/PASSKEY/TERM  (3 fields, no reg_port)
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

    // Parse "PORT <number>" from etrs stdout.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(port_str) = line.trim().strip_prefix("PORT ")
            && let Ok(port) = port_str.trim().parse::<u16>()
        {
            return Ok(port);
        }
    }
    Err(io::Error::other(format!(
        "etrs did not report a port (stdout: {:?})",
        stdout.trim()
    )))
}

async fn run_connection_loop(
    target: String,
    port: u16,
    passkey: String,
    session_id: [u8; 16],
    session: Arc<Mutex<SessionState>>,
    verbose: u8,
) -> io::Result<()> {
    let host = if let Some(idx) = target.find('@') {
        &target[idx + 1..]
    } else {
        &target
    };
    let server_addr = tokio::net::lookup_host(format!("{}:{}", host, port))
        .await?
        .next()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("could not resolve host: {}", host),
            )
        })?;

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
            vlog!(verbose, 1, "\r\n[etr] Reconnecting to {}...", server_addr);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        first = false;

        let bind_addr = if server_addr.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        let socket = match UdpSocket::bind(bind_addr).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                eprintln!("[etr] Bind error: {:?}", e);
                continue;
            }
        };

        let last_received = {
            let s = session.lock().await;
            s.last_received_map()
        };

        vlog!(
            verbose,
            2,
            "[etr] Sending ClientHello  session={} suites={:?}",
            hex_encode(&session_id),
            CipherSuiteId::client_preference()
        );

        let (hs, hello_header, hello_envelope) =
            ClientHandshake::reconnect(session_id, passkey.clone(), last_received);

        if let Err(e) =
            send_packet(&socket, server_addr, &hello_header, &hello_envelope, None).await
        {
            vlog!(verbose, 1, "[etr] Failed to send ClientHello: {:?}", e);
            continue;
        }

        // Wait for ServerHello (with timeout).
        let pkt = match tokio::time::timeout(Duration::from_secs(10), recv_packet(&socket)).await {
            Ok(Ok(Some(p))) => p,
            _ => {
                vlog!(verbose, 1, "[etr] ServerHello timeout");
                continue;
            }
        };

        if !pkt.header.is_handshake() || pkt.header.session_id != session_id {
            continue;
        }

        let (cipher, suite, server_acks) = match hs.process_server_hello(&pkt.payload_bytes) {
            Ok(r) => r,
            Err(e) => {
                vlog!(verbose, 1, "[etr] Handshake failed: {}", e);
                continue;
            }
        };

        vlog!(
            verbose,
            2,
            "[etr] Handshake complete  suite={}  session={}",
            suite,
            hex_encode(&session_id)
        );

        let cipher = Arc::new(cipher);

        vlog!(verbose, 1, "\r\n[etr] Connected. Session active.");

        {
            let mut s = session.lock().await;
            s.apply_server_acks(&server_acks);
            s.cipher = None; // replaced below per-connection
        }

        enable_raw_mode().unwrap();
        IN_RAW_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
        let result = run_session(
            socket,
            server_addr,
            session_id,
            Arc::clone(&cipher),
            Arc::clone(&session),
            Arc::clone(&stdin_rx),
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
                vlog!(verbose, 1, "[etr] Session dropped: {:?}", e);
            }
        }
    }
}

async fn run_session(
    socket: Arc<UdpSocket>,
    server_addr: SocketAddr,
    session_id: [u8; 16],
    cipher: Arc<etr::crypto::AeadCipher>,
    session: Arc<Mutex<SessionState>>,
    stdin_rx: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
    verbose: u8,
) -> io::Result<()> {
    let (send_tx, mut send_rx) = mpsc::channel::<Envelope>(1000);

    // Replay any unacknowledged outbound data.
    {
        let s = session.lock().await;
        let peer_acks: HashMap<u32, u64> = HashMap::new();
        for (stream_id, replays) in s.collect_replays(&peer_acks) {
            for (seq, data) in replays {
                let _ = send_tx
                    .send(Envelope {
                        payload: Some(Payload::StreamData(StreamData {
                            stream_id,
                            seq_num: seq,
                            data,
                        })),
                    })
                    .await;
            }
        }
    }

    // Send current terminal size.
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        let _ = send_tx
            .send(Envelope {
                payload: Some(Payload::TerminalResize(TerminalResize {
                    rows: rows as u32,
                    cols: cols as u32,
                })),
            })
            .await;
    }

    // Task: write outbound packets to the socket.
    let socket_w = Arc::clone(&socket);
    let cipher_w = Arc::clone(&cipher);
    let session_w = Arc::clone(&session);
    let mut writer_task = tokio::spawn(async move {
        while let Some(envelope) = send_rx.recv().await {
            let seq = {
                let mut s = session_w.lock().await;
                s.next_packet_seq()
            };
            let header = PacketHeader::new(0, session_id, seq);
            vlog!(
                verbose,
                3,
                "[etr] → {} seq={} {}b",
                payload_type(envelope.payload.as_ref()),
                seq,
                envelope.encoded_len()
            );
            let _ = send_packet(&socket_w, server_addr, &header, &envelope, Some(&cipher_w)).await;
        }
    });

    // Task: forward stdin to the server as StreamData on stream 0.
    let session_stdin = Arc::clone(&session);
    let send_tx_stdin = send_tx.clone();
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
            let _ = send_tx_stdin
                .send(Envelope {
                    payload: Some(Payload::StreamData(StreamData {
                        stream_id: 0,
                        seq_num: seq,
                        data: payload,
                    })),
                })
                .await;
        }
    });

    // Task: receive packets from the server and write terminal output to stdout.
    let socket_r = Arc::clone(&socket);
    let cipher_r = Arc::clone(&cipher);
    let session_r = Arc::clone(&session);
    let mut reader_task = tokio::spawn(async move {
        let mut stdout = io::stdout();
        loop {
            let pkt =
                match tokio::time::timeout(Duration::from_secs(15), recv_packet(&socket_r)).await {
                    Ok(Ok(Some(p))) => p,
                    Ok(Ok(None)) => continue,
                    Ok(Err(e)) => return Err(e),
                    Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "idle timeout")),
                };

            if pkt.header.session_id != session_id {
                continue;
            }

            let envelope = if pkt.header.is_handshake() {
                decode_plaintext_packet(&pkt.payload_bytes)?
            } else {
                decode_data_packet(&pkt.payload_bytes, pkt.header.packet_seq, &cipher_r)?
            };

            vlog!(
                verbose,
                3,
                "[etr] ← {} seq={} {}b",
                payload_type(envelope.payload.as_ref()),
                pkt.header.packet_seq,
                pkt.payload_bytes.len()
            );

            match envelope.payload {
                Some(Payload::StreamData(sd)) if sd.stream_id == 0 => {
                    let expected = {
                        let s = session_r.lock().await;
                        s.stream(0).map(|st| st.next_in_seq).unwrap_or(1)
                    };
                    if sd.seq_num == expected {
                        let _ = stdout.write_all(&sd.data);
                        let _ = stdout.flush();
                        let mut s = session_r.lock().await;
                        if let Some(st) = s.stream_mut(0) {
                            st.next_in_seq += 1;
                        }
                    }
                }
                Some(Payload::Disconnect(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "clean disconnect from server",
                    ));
                }
                Some(Payload::Heartbeat(_)) => {
                    vlog!(verbose, 3, "[etr] ← heartbeat");
                }
                _ => {}
            }
        }
    });

    // Task: send SIGWINCH resize events.
    let send_tx_resize = send_tx.clone();
    let mut resize_task = tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut sigwinch) = signal(SignalKind::window_change()) {
            while sigwinch.recv().await.is_some() {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    let _ = send_tx_resize
                        .send(Envelope {
                            payload: Some(Payload::TerminalResize(TerminalResize {
                                rows: rows as u32,
                                cols: cols as u32,
                            })),
                        })
                        .await;
                }
            }
        }
    });

    // Task: heartbeats every 5 s.
    let send_tx_hb = send_tx.clone();
    let mut hb_task = tokio::spawn(async move {
        let mut hb_count: u64 = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            hb_count += 1;
            vlog!(verbose, 3, "[etr] → heartbeat #{}", hb_count);
            if send_tx_hb
                .send(Envelope {
                    payload: Some(Payload::Heartbeat(Heartbeat {})),
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let result = tokio::select! {
        _ = &mut writer_task  => Ok(()),
        _ = &mut stdin_task   => Ok(()),
        r = &mut reader_task  => r.unwrap_or_else(|e| Err(io::Error::other(e.to_string()))),
        _ = &mut resize_task  => Ok(()),
        _ = &mut hb_task      => Ok(()),
    };

    writer_task.abort();
    stdin_task.abort();
    reader_task.abort();
    resize_task.abort();
    hb_task.abort();

    result
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
        // target is now passed as-is to SSH; no port stripping needed.
        let cli = Cli::try_parse_from(["etr", "user@host"]).unwrap();
        assert_eq!(cli.target.as_deref(), Some("user@host"));
        let cli = Cli::try_parse_from(["etr", "localhost"]).unwrap();
        assert_eq!(cli.target.as_deref(), Some("localhost"));
    }
}
