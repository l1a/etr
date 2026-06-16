// SPDX-License-Identifier: GPL-3.0-or-later
use clap::{ArgAction, Parser, Subcommand};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::HashMap;
use std::io::IsTerminal;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UdpSocket, UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};

static VERBOSITY: std::sync::OnceLock<u8> = std::sync::OnceLock::new();

fn verbosity() -> u8 {
    *VERBOSITY.get().unwrap_or(&0)
}

/// Log to stderr if the global verbosity level is >= `$level`.
macro_rules! vlog {
    ($level:expr, $($arg:tt)*) => {
        if verbosity() >= $level {
            eprintln!($($arg)*);
        }
    };
}

fn payload_type(p: Option<&etr::protocol::Payload>) -> &'static str {
    use etr::protocol::Payload;
    match p {
        Some(Payload::ClientHello(_))    => "ClientHello",
        Some(Payload::ServerHello(_))    => "ServerHello",
        Some(Payload::StreamOpen(_))     => "StreamOpen",
        Some(Payload::StreamClose(_))    => "StreamClose",
        Some(Payload::StreamData(_))     => "StreamData",
        Some(Payload::StreamAck(_))      => "StreamAck",
        Some(Payload::TerminalResize(_)) => "TerminalResize",
        Some(Payload::Heartbeat(_))      => "Heartbeat",
        Some(Payload::Disconnect(_))     => "Disconnect",
        None                             => "Empty",
    }
}

fn default_socket_path() -> String {
    dirs::runtime_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("etr")
        .join("etrs.sock")
        .to_string_lossy()
        .into_owned()
}

use etr::handshake::process_client_hello;
use etr::protocol::{
    Disconnect, Envelope, Heartbeat, PacketHeader, Payload, StreamData,
};
use etr::session::SessionState;
use etr::transport::{ReceivedPacket, decode_data_packet, recv_packet, send_packet};

#[derive(Parser)]
#[command(
    name = "etrs",
    version = "0.2.0",
    about = "Eternal Terminal Server Daemon in Rust"
)]
struct Cli {
    /// UDP port the daemon listens on.
    #[arg(short, long, default_value = "2022")]
    port: u16,

    /// IP address to bind the UDP listener to.
    #[arg(short, long, default_value = "0.0.0.0")]
    bind: String,

    /// Path to the Unix domain socket used for local session registration.
    #[arg(short, long, default_value_t = default_socket_path())]
    socket: String,

    /// Verbosity: -v session events, -vv cipher details, -vvv packet trace
    #[arg(short = 'v', action = ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the persistent background daemon
    Daemon,
    /// Register a new session (typically invoked via SSH)
    Register,
}

/// State shared between the registration path and the UDP demux loop for one session.
struct ActiveSession {
    passkey: String,
    session_state: Arc<Mutex<SessionState>>,
    pty_write_tx: mpsc::Sender<Vec<u8>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// Inbound packet channel: the demux loop sends here; the connection handler reads here.
    /// Replaced on each reconnect, which drops the old Sender and closes the old Receiver.
    inbound_tx: std::sync::Mutex<Option<mpsc::Sender<ReceivedPacket>>>,
    /// Outbound envelope channel: PTY reader sends here; the connection handler encrypts + sends.
    outbound_tx: std::sync::Mutex<Option<mpsc::Sender<Envelope>>>,
}

type SessionMap = Arc<std::sync::Mutex<HashMap<[u8; 16], Arc<ActiveSession>>>>;

#[tokio::main]
async fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let _ = VERBOSITY.set(cli.verbose);
    let cmd = cli.command.unwrap_or_else(|| {
        if !io::stdin().is_terminal() { Commands::Register } else { Commands::Daemon }
    });
    match cmd {
        Commands::Daemon => run_daemon(cli.bind, cli.port, cli.socket).await,
        Commands::Register => run_register(cli.socket).await,
    }
}

async fn run_register(socket_path: String) -> io::Result<()> {
    // run_register is invoked via SSH; keep its output minimal.
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();
    let parts: Vec<&str> = input.split('/').collect();
    if parts.len() < 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Expected SESSION_ID_HEX/PASSKEY/TERM",
        ));
    }
    let mut stream = UnixStream::connect(&socket_path).await?;
    stream
        .write_all(format!("{}/{}/{}\n", parts[0], parts[1], parts[2]).as_bytes())
        .await?;
    stream.flush().await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let response = String::from_utf8_lossy(&buf);
    if response.trim() == "OK" {
        vlog!(1, "Session registered successfully.");
        Ok(())
    } else {
        Err(io::Error::other(format!("Registration failed: {}", response)))
    }
}

async fn run_daemon(bind_addr: String, port: u16, socket_path: String) -> io::Result<()> {
    eprintln!("Starting etrs daemon...");
    let sessions: SessionMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
    if let Some(parent) = std::path::Path::new(&socket_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&socket_path);

    // Unix socket for SSH-bootstrapped registration.
    let unix_listener = UnixListener::bind(&socket_path)?;
    let sessions_reg = Arc::clone(&sessions);
    tokio::spawn(async move {
        while let Ok((stream, _)) = unix_listener.accept().await {
            let sess = Arc::clone(&sessions_reg);
            tokio::spawn(async move {
                if let Err(e) = handle_registration(stream, sess).await {
                    eprintln!("[etrs] Registration error: {:?}", e);
                }
            });
        }
    });

    // Single UDP socket — this loop is the exclusive reader.
    let addr: SocketAddr = format!("{}:{}", bind_addr, port)
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let socket = Arc::new(UdpSocket::bind(&addr).await?);
    eprintln!("Listening on UDP {}", addr);

    loop {
        let pkt = match recv_packet(&socket).await? {
            Some(p) => p,
            None => continue, // unknown protocol version; skip
        };

        if pkt.header.is_handshake() {
            // New connection or reconnect: spawn a handler for the handshake.
            let sess = Arc::clone(&sessions);
            let sock = Arc::clone(&socket);
            tokio::spawn(async move {
                if let Err(e) =
                    handle_client_hello(pkt.peer, pkt.header, pkt.payload_bytes, sess, sock).await
                {
                    vlog!(1, "[etrs] Handshake error from {}: {:?}", pkt.peer, e);
                }
            });
        } else {
            // Data packet: route to the session's inbound channel.
            let session_id = pkt.header.session_id;
            let tx = sessions
                .lock()
                .unwrap()
                .get(&session_id)
                .and_then(|s| s.inbound_tx.lock().unwrap().clone());
            if let Some(tx) = tx {
                // Non-blocking: drop the packet if the handler is behind.
                let _ = tx.try_send(pkt);
            }
        }
    }
}

async fn handle_registration(mut stream: UnixStream, sessions: SessionMap) -> io::Result<()> {
    let mut buf = vec![0u8; 1024];
    let n = stream.read(&mut buf).await?;
    let msg = String::from_utf8_lossy(&buf[..n]);
    let parts: Vec<&str> = msg.trim().split('/').collect();
    if parts.len() < 3 {
        stream.write_all(b"ERROR: Invalid format").await?;
        return Ok(());
    }
    let session_id: [u8; 16] = hex_decode(parts[0])
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| io::Error::other("Invalid session_id hex"))?;
    let passkey = parts[1].to_string();
    let term = parts[2].to_string();

    vlog!(1, "[etrs] Registering session id={} term={}", parts[0], term);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .map_err(io::Error::other)?;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let mut cmd = CommandBuilder::new(shell);
    cmd.env("TERM", term);
    let mut child = pair.slave.spawn_command(cmd).map_err(io::Error::other)?;

    let master = pair.master;
    let mut pty_reader = master.try_clone_reader().map_err(io::Error::other)?;
    let mut pty_writer = master.take_writer().map_err(io::Error::other)?;

    let (pty_write_tx, mut pty_write_rx) = mpsc::channel::<Vec<u8>>(1000);
    tokio::task::spawn_blocking(move || {
        while let Some(data) = pty_write_rx.blocking_recv() {
            if pty_writer.write_all(&data).is_err() {
                break;
            }
            let _ = pty_writer.flush();
        }
    });

    let master_shared = Arc::new(Mutex::new(master));
    let session_state = Arc::new(Mutex::new(SessionState::new(session_id, passkey.clone())));

    let active = Arc::new(ActiveSession {
        passkey,
        session_state: Arc::clone(&session_state),
        pty_write_tx,
        master: Arc::clone(&master_shared),
        inbound_tx: std::sync::Mutex::new(None),
        outbound_tx: std::sync::Mutex::new(None),
    });

    // Clean up on shell exit.
    let sessions_cleanup = Arc::clone(&sessions);
    let active_cleanup = Arc::clone(&active);
    tokio::task::spawn_blocking(move || {
        let _ = child.wait();
        vlog!(1, "[etrs] Shell exited for session {}, cleaning up.", hex_encode(&session_id));
        sessions_cleanup.lock().unwrap().remove(&session_id);
        // Signal any connected client to disconnect.
        let tx = active_cleanup.outbound_tx.lock().unwrap().clone();
        if let Some(tx) = tx {
            let _ = tx.blocking_send(Envelope {
                payload: Some(Payload::Disconnect(Disconnect {})),
            });
        }
    });

    // Forward PTY output to any connected client via the outbound channel.
    let session_state_pty = Arc::clone(&session_state);
    let active_pty = Arc::clone(&active);
    let sessions_pty = Arc::clone(&sessions);
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = pty_reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            let payload = buf[..n].to_vec();
            let seq = {
                let mut s = futures::executor::block_on(session_state_pty.lock());
                let st = s.stream_mut(0).expect("stream 0 always exists");
                let seq = st.next_out_seq;
                st.next_out_seq += 1;
                st.record_send(seq, payload.clone());
                seq
            };
            let tx = active_pty.outbound_tx.lock().unwrap().clone();
            if let Some(tx) = tx {
                let _ = tx.blocking_send(Envelope {
                    payload: Some(Payload::StreamData(StreamData {
                        stream_id: 0,
                        seq_num: seq,
                        data: payload,
                    })),
                });
            }
        }
        vlog!(2, "[etrs] PTY reader exited for session {}.", hex_encode(&session_id));
        sessions_pty.lock().unwrap().remove(&session_id);
    });

    sessions.lock().unwrap().insert(session_id, active);
    stream.write_all(b"OK").await?;
    Ok(())
}

async fn handle_client_hello(
    peer: SocketAddr,
    header: PacketHeader,
    payload_bytes: Vec<u8>,
    sessions: SessionMap,
    socket: Arc<UdpSocket>,
) -> io::Result<()> {
    let session_id = header.session_id;

    // Look up passkey and active session — drop the std Mutex guard before any .await.
    let (passkey, active) = {
        let guard = sessions.lock().unwrap();
        let s = guard
            .get(&session_id)
            .ok_or_else(|| io::Error::other("unknown session"))?;
        (s.passkey.clone(), Arc::clone(s))
        // guard dropped here
    };
    let server_last_received = active.session_state.lock().await.last_received_map();

    vlog!(1, "[etrs] ClientHello from {}  session={}", peer, hex_encode(&session_id));

    let outcome = process_client_hello(
        &payload_bytes,
        server_last_received,
        |_| Some(passkey.clone()),
    )
    .map_err(|e| io::Error::other(e.to_string()))?;

    vlog!(2, "[etrs] Handshake complete  suite={}  session={}  peer={}",
        outcome.chosen_suite, hex_encode(&outcome.session_id), peer);

    // Apply client acks and collect replay packets.
    let replays = {
        let mut s = active.session_state.lock().await;
        s.apply_server_acks(&outcome.client_last_received);
        s.collect_replays(&outcome.client_last_received)
    };

    // Send ServerHello (pre-encrypted with the hello key).
    let mut buf =
        Vec::with_capacity(etr::protocol::HEADER_SIZE + outcome.response_payload_bytes.len());
    buf.extend_from_slice(&outcome.response_header.encode());
    buf.extend_from_slice(&outcome.response_payload_bytes);
    socket.send_to(&buf, peer).await?;

    let cipher = Arc::new(outcome.cipher);

    // Create per-connection inbound channel and register it so the demux loop
    // can route data packets here.  Replacing the Sender closes the previous
    // Receiver, which cleanly terminates any prior connection's reader task.
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<ReceivedPacket>(256);
    *active.inbound_tx.lock().unwrap() = Some(inbound_tx);

    // Create per-connection outbound channel and wire the PTY reader into it.
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Envelope>(1000);
    *active.outbound_tx.lock().unwrap() = Some(outbound_tx);

    // Queue replay packets ahead of live data.
    for (stream_id, packets) in replays {
        for (seq, data) in packets {
            if active
                .outbound_tx
                .lock()
                .unwrap()
                .as_ref()
                .map(|tx| {
                    tx.try_send(Envelope {
                        payload: Some(Payload::StreamData(StreamData {
                            stream_id,
                            seq_num: seq,
                            data,
                        })),
                    })
                    .is_ok()
                })
                .unwrap_or(false)
            {}
        }
    }

    let session_id = outcome.session_id;
    let socket_w = Arc::clone(&socket);
    let cipher_w = Arc::clone(&cipher);
    let session_w = Arc::clone(&active.session_state);

    // Writer task: encrypt and send each outbound envelope to the client.
    let mut writer = tokio::spawn(async move {
        use prost::Message as _;
        while let Some(envelope) = outbound_rx.recv().await {
            let seq = {
                let mut s = session_w.lock().await;
                s.next_packet_seq()
            };
            vlog!(3, "[etrs] → {} seq={} {}b  peer={}",
                payload_type(envelope.payload.as_ref()), seq, envelope.encoded_len(), peer);
            let header = PacketHeader::new(0, session_id, seq);
            let _ = send_packet(&socket_w, peer, &header, &envelope, Some(&cipher_w)).await;
        }
    });

    // Reader task: drain the inbound channel (filled by the demux loop) and
    // dispatch to PTY / resize / control handlers.
    let cipher_r = Arc::clone(&cipher);
    let session_r = Arc::clone(&active.session_state);
    let pty_tx = active.pty_write_tx.clone();
    let master = Arc::clone(&active.master);
    let mut reader = tokio::spawn(async move {
        loop {
            let pkt = match tokio::time::timeout(
                std::time::Duration::from_secs(15),
                inbound_rx.recv(),
            )
            .await
            {
                Ok(Some(p)) => p,
                Ok(None) => {
                    vlog!(1, "[etrs] Connection replaced for {}  session={}",
                        peer, hex_encode(&session_id));
                    break; // channel replaced by a reconnect; exit cleanly
                }
                Err(_) => {
                    vlog!(1, "[etrs] Client {} timed out  session={}", peer, hex_encode(&session_id));
                    break;
                }
            };

            // A handshake packet arriving here means the client is reconnecting;
            // the new handle_client_hello call will replace our inbound channel.
            if pkt.header.is_handshake() {
                break;
            }

            let envelope = match decode_data_packet(
                &pkt.payload_bytes,
                pkt.header.packet_seq,
                &cipher_r,
            ) {
                Ok(e) => e,
                Err(_) => continue,
            };

            vlog!(3, "[etrs] ← {} seq={} {}b  peer={}",
                payload_type(envelope.payload.as_ref()),
                pkt.header.packet_seq,
                pkt.payload_bytes.len(),
                peer);

            match envelope.payload {
                Some(Payload::StreamData(sd)) if sd.stream_id == 0 => {
                    let expected = {
                        let s = session_r.lock().await;
                        s.stream(0).map(|st| st.next_in_seq).unwrap_or(1)
                    };
                    if sd.seq_num == expected {
                        let _ = pty_tx.send(sd.data).await;
                        let mut s = session_r.lock().await;
                        if let Some(st) = s.stream_mut(0) {
                            st.next_in_seq += 1;
                        }
                    }
                }
                Some(Payload::TerminalResize(tr)) => {
                    let m = master.lock().await;
                    let _ = m.resize(PtySize {
                        rows: tr.rows as u16,
                        cols: tr.cols as u16,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                Some(Payload::Disconnect(_)) => break,
                Some(Payload::Heartbeat(_)) => {}
                _ => {}
            }
        }
        Ok::<_, io::Error>(())
    });

    // Heartbeat task.
    let outbound_hb = active.outbound_tx.lock().unwrap().clone();
    let mut hb_task = tokio::spawn(async move {
        if let Some(tx) = outbound_hb {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if tx
                    .send(Envelope { payload: Some(Payload::Heartbeat(Heartbeat {})) })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });

    tokio::select! {
        _ = &mut writer  => {}
        _ = &mut reader  => {}
        _ = &mut hb_task => {}
    }
    writer.abort();
    reader.abort();
    hb_task.abort();

    // Clear the channels so the PTY reader stops queuing into the dead connection.
    *active.inbound_tx.lock().unwrap() = None;
    *active.outbound_tx.lock().unwrap() = None;

    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
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
    use clap::CommandFactory;

    #[test]
    fn test_cli_defaults() {
        let cli = Cli::try_parse_from(["etrs"]).unwrap();
        assert_eq!(cli.port, 2022);
        assert_eq!(cli.bind, "0.0.0.0");
        assert_eq!(cli.socket, default_socket_path());
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
    fn test_cli_daemon_subcommand() {
        let cli = Cli::try_parse_from(["etrs", "daemon"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Daemon)));
    }

    #[test]
    fn test_cli_register_subcommand() {
        let cli = Cli::try_parse_from(["etrs", "register"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Register)));
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
}
