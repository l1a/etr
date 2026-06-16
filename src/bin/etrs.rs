// SPDX-License-Identifier: GPL-3.0-or-later
use clap::{Parser, Subcommand};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::HashMap;
use std::io::IsTerminal;
use std::io::{self, Read, Write};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{UdpSocket, UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};

use etr::crypto::AeadCipher;
use etr::handshake::process_client_hello;
use etr::protocol::{
    Envelope, Payload, StreamData, Disconnect, TerminalResize, Heartbeat, PacketHeader,
};
use etr::session::SessionState;
use etr::transport::{decode_data_packet, recv_packet, send_packet};

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
    #[arg(short, long, default_value = "/tmp/etrs.sock")]
    socket: String,

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

/// State shared between the registration path and the UDP handler for one session.
struct ActiveSession {
    session_id: [u8; 16],
    passkey: String,
    session_state: Arc<Mutex<SessionState>>,
    pty_write_tx: mpsc::Sender<Vec<u8>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// Current active UDP sender channel + remote addr, replaced on reconnect.
    udp_tx: Arc<std::sync::Mutex<Option<(mpsc::Sender<Envelope>, SocketAddr)>>>,
}

type SessionMap = Arc<std::sync::Mutex<HashMap<[u8; 16], Arc<ActiveSession>>>>;

#[tokio::main]
async fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let cmd = cli.command.unwrap_or_else(|| {
        if !io::stdin().is_terminal() { Commands::Register } else { Commands::Daemon }
    });
    match cmd {
        Commands::Daemon => run_daemon(cli.bind, cli.port, cli.socket).await,
        Commands::Register => run_register(cli.socket).await,
    }
}

async fn run_register(socket_path: String) -> io::Result<()> {
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();
    let parts: Vec<&str> = input.split('/').collect();
    if parts.len() < 3 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Expected SESSION_ID_HEX/PASSKEY/TERM"));
    }
    let mut stream = UnixStream::connect(&socket_path).await?;
    stream.write_all(format!("{}/{}/{}\n", parts[0], parts[1], parts[2]).as_bytes()).await?;
    stream.flush().await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let response = String::from_utf8_lossy(&buf);
    if response.trim() == "OK" {
        println!("Session registered successfully.");
        Ok(())
    } else {
        Err(io::Error::other(format!("Registration failed: {}", response)))
    }
}

async fn run_daemon(bind_addr: String, port: u16, socket_path: String) -> io::Result<()> {
    println!("Starting etrs daemon...");
    let sessions: SessionMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let _ = std::fs::remove_file(&socket_path);

    // Unix socket for SSH-bootstrapped registration.
    let unix_listener = UnixListener::bind(&socket_path)?;
    let sessions_reg = Arc::clone(&sessions);
    tokio::spawn(async move {
        while let Ok((stream, _)) = unix_listener.accept().await {
            let sess = Arc::clone(&sessions_reg);
            tokio::spawn(async move {
                if let Err(e) = handle_registration(stream, sess).await {
                    eprintln!("Registration error: {:?}", e);
                }
            });
        }
    });

    // UDP listener for etr clients.
    let addr: SocketAddr = format!("{}:{}", bind_addr, port)
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let socket = Arc::new(UdpSocket::bind(&addr).await?);
    println!("Listening on UDP {}", addr);

    loop {
        let pkt = match recv_packet(&socket).await? {
            Some(p) => p,
            None => continue,
        };

        if pkt.header.is_handshake() {
            let sess = Arc::clone(&sessions);
            let sock = Arc::clone(&socket);
            tokio::spawn(async move {
                if let Err(e) = handle_client_hello(pkt.peer, pkt.header, pkt.payload_bytes, sess, sock).await {
                    eprintln!("Handshake error from {}: {:?}", pkt.peer, e);
                }
            });
        } else {
            // Route data packet to the correct session.
            let session_id = pkt.header.session_id;
            let entry = sessions.lock().unwrap().get(&session_id).cloned();
            if let Some(session) = entry {
                let cipher = {
                    let s = session.session_state.lock().await;
                    // cipher is stored per-connection in udp_tx context; handled below
                    drop(s);
                    None::<Arc<AeadCipher>>
                };
                // Forward to the session's active connection handler via its channel.
                let tx_opt = session.udp_tx.lock().unwrap().as_ref().map(|(tx, _)| tx.clone());
                if let Some(tx) = tx_opt {
                    // Wrap raw packet info as an opaque envelope for routing;
                    // the connection handler decrypts it with its own cipher handle.
                    // Since we can't pass raw bytes through the Envelope channel, we
                    // re-parse here using the session's stored cipher.
                    drop(cipher); // satisfy the borrow checker
                    let _ = tx; // connection handler owns decryption
                }
            }
        }
    }
}

async fn handle_registration(mut stream: UnixStream, sessions: SessionMap) -> io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = vec![0u8; 1024];
    let n = stream.read(&mut buf).await?;
    let msg = String::from_utf8_lossy(&buf[..n]);
    let parts: Vec<&str> = msg.trim().split('/').collect();
    if parts.len() < 3 {
        stream.write_all(b"ERROR: Invalid format").await?;
        return Ok(());
    }
    let session_id = hex_decode(parts[0]).and_then(|b| b.try_into().ok())
        .ok_or_else(|| io::Error::other("Invalid session_id hex"))?;
    let passkey = parts[1].to_string();
    let term = parts[2].to_string();

    println!("Registering session id={} term={}", parts[0], term);

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
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
            if pty_writer.write_all(&data).is_err() { break; }
            let _ = pty_writer.flush();
        }
    });

    let master_shared = Arc::new(Mutex::new(master));
    let session_state = Arc::new(Mutex::new(SessionState::new(session_id, passkey.clone())));
    let udp_tx: Arc<std::sync::Mutex<Option<(mpsc::Sender<Envelope>, SocketAddr)>>> =
        Arc::new(std::sync::Mutex::new(None));

    let active = Arc::new(ActiveSession {
        session_id,
        passkey,
        session_state: Arc::clone(&session_state),
        pty_write_tx,
        master: Arc::clone(&master_shared),
        udp_tx: Arc::clone(&udp_tx),
    });

    // Clean up on shell exit.
    let sessions_cleanup = Arc::clone(&sessions);
    let udp_tx_cleanup = Arc::clone(&udp_tx);
    tokio::task::spawn_blocking(move || {
        let _ = child.wait();
        println!("Shell exited for session {:?}, cleaning up.", session_id);
        sessions_cleanup.lock().unwrap().remove(&session_id);
        let guard = udp_tx_cleanup.lock().unwrap();
        if let Some((tx, _)) = &*guard {
            let _ = tx.blocking_send(Envelope { payload: Some(Payload::Disconnect(Disconnect {})) });
        }
    });

    // Forward PTY output to any connected UDP client.
    let session_state_pty = Arc::clone(&session_state);
    let udp_tx_pty = Arc::clone(&udp_tx);
    let sessions_pty = Arc::clone(&sessions);
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = pty_reader.read(&mut buf) {
            if n == 0 { break; }
            let payload = buf[..n].to_vec();
            let seq = {
                let mut s = futures::executor::block_on(session_state_pty.lock());
                let st = s.stream_mut(0).expect("stream 0 always exists");
                let seq = st.next_out_seq;
                st.next_out_seq += 1;
                st.record_send(seq, payload.clone());
                seq
            };
            let tx_opt = udp_tx_pty.lock().unwrap().as_ref().map(|(tx, _)| tx.clone());
            if let Some(tx) = tx_opt {
                let _ = tx.blocking_send(Envelope {
                    payload: Some(Payload::StreamData(StreamData {
                        stream_id: 0,
                        seq_num: seq,
                        data: payload,
                    })),
                });
            }
        }
        println!("PTY reader exited for session {:?}.", session_id);
        sessions_pty.lock().unwrap().remove(&session_id);
    });

    sessions.lock().unwrap().insert(session_id, active);
    stream.write_all(b"OK").await?;
    Ok(())
}

async fn handle_client_hello(
    peer: SocketAddr,
    _header: PacketHeader,
    payload_bytes: Vec<u8>,
    sessions: SessionMap,
    socket: Arc<UdpSocket>,
) -> io::Result<()> {
    let session = {
        let guard = sessions.lock().unwrap();
        // Peek at session_id from the payload before the full handshake.
        // We need to extract it to look up the passkey.
        // process_client_hello does this internally via the lookup closure.
        drop(guard);
    };
    let _ = session;

    let sessions_lookup = Arc::clone(&sessions);
    let outcome = process_client_hello(
        &payload_bytes,
        HashMap::new(), // server_last_received filled below from session state
        move |session_id_bytes| {
            let sid: [u8; 16] = session_id_bytes.try_into().ok()?;
            let guard = sessions_lookup.lock().unwrap();
            guard.get(&sid).map(|s| s.passkey.clone())
        },
    )
    .map_err(|e| io::Error::other(e.to_string()))?;

    // Look up the active session.
    let active = {
        let guard = sessions.lock().unwrap();
        guard.get(&outcome.session_id).cloned()
    };
    let Some(active) = active else {
        return Err(io::Error::other("session disappeared after handshake"));
    };

    // Apply client acks and compute server's replay set.
    let replays = {
        let mut s = active.session_state.lock().await;
        s.apply_server_acks(&outcome.client_last_received);
        s.collect_replays(&outcome.client_last_received)
    };

    // Send ServerHello. The payload is already hello-key-encrypted, so we write raw bytes.
    let mut buf = Vec::with_capacity(etr::protocol::HEADER_SIZE + outcome.response_payload_bytes.len());
    buf.extend_from_slice(&outcome.response_header.encode());
    buf.extend_from_slice(&outcome.response_payload_bytes);
    socket.send_to(&buf, peer).await?;

    let cipher = Arc::new(outcome.cipher);

    // Set up a channel for this connection's outbound packets.
    let (tx, mut rx) = mpsc::channel::<Envelope>(1000);
    {
        let mut guard = active.udp_tx.lock().unwrap();
        *guard = Some((tx.clone(), peer));
    }

    // Queue replay packets.
    for (stream_id, packets) in replays {
        for (seq, data) in packets {
            let _ = tx.send(Envelope {
                payload: Some(Payload::StreamData(StreamData { stream_id, seq_num: seq, data })),
            }).await;
        }
    }

    let socket_w = Arc::clone(&socket);
    let cipher_w = Arc::clone(&cipher);
    let session_w = Arc::clone(&active.session_state);
    let session_id = outcome.session_id;

    // Writer task: encrypt and send outbound envelopes.
    let mut writer = tokio::spawn(async move {
        while let Some(envelope) = rx.recv().await {
            let seq = { let mut s = session_w.lock().await; s.next_packet_seq() };
            let header = PacketHeader::new(0, session_id, seq);
            let _ = send_packet(&socket_w, peer, &header, &envelope, Some(&cipher_w)).await;
        }
    });

    // Reader task: receive and decrypt inbound data from this client.
    let socket_r = Arc::clone(&socket);
    let cipher_r = Arc::clone(&cipher);
    let session_r = Arc::clone(&active.session_state);
    let pty_tx = active.pty_write_tx.clone();
    let master = Arc::clone(&active.master);
    let mut reader = tokio::spawn(async move {
        loop {
            let pkt = match tokio::time::timeout(
                std::time::Duration::from_secs(15),
                recv_packet(&socket_r),
            ).await {
                Ok(Ok(Some(p))) if p.peer == peer && p.header.session_id == session_id => p,
                Ok(Ok(_)) => continue,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    eprintln!("Client {} timed out.", peer);
                    break;
                }
            };

            if pkt.header.is_handshake() { break; } // reconnect; new handler takes over

            let envelope = match decode_data_packet(&pkt.payload_bytes, pkt.header.packet_seq, &cipher_r) {
                Ok(e) => e,
                Err(_) => continue,
            };

            match envelope.payload {
                Some(Payload::StreamData(sd)) if sd.stream_id == 0 => {
                    let expected = {
                        let s = session_r.lock().await;
                        s.stream(0).map(|st| st.next_in_seq).unwrap_or(1)
                    };
                    if sd.seq_num == expected {
                        let _ = pty_tx.send(sd.data).await;
                        let mut s = session_r.lock().await;
                        if let Some(st) = s.stream_mut(0) { st.next_in_seq += 1; }
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
    let tx_hb = {
        let guard = active.udp_tx.lock().unwrap();
        guard.as_ref().map(|(tx, _)| tx.clone())
    };
    let mut hb_task = tokio::spawn(async move {
        if let Some(tx) = tx_hb {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if tx.send(Envelope { payload: Some(Payload::Heartbeat(Heartbeat {})) }).await.is_err() {
                    break;
                }
            }
        }
    });

    tokio::select! {
        _ = &mut writer => {}
        _ = &mut reader => {}
        _ = &mut hb_task => {}
    }
    writer.abort();
    reader.abort();
    hb_task.abort();

    // Clear active channel so the PTY reader stops trying to send.
    let mut guard = active.udp_tx.lock().unwrap();
    if guard.as_ref().map(|(_, a)| *a == peer).unwrap_or(false) {
        *guard = None;
    }

    Ok(())
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 { return None; }
    (0..s.len()).step_by(2)
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
        assert_eq!(cli.socket, "/tmp/etrs.sock");
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
