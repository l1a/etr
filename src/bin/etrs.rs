// SPDX-License-Identifier: GPL-3.0-or-later
use clap::{ArgAction, Parser};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc};

static VERBOSITY: std::sync::OnceLock<u8> = std::sync::OnceLock::new();

fn verbosity() -> u8 {
    *VERBOSITY.get().unwrap_or(&0)
}

macro_rules! vlog {
    ($level:expr, $($arg:tt)*) => {
        if verbosity() >= $level { eprintln!($($arg)*); }
    };
}

fn payload_type(p: Option<&etr::protocol::Payload>) -> &'static str {
    use etr::protocol::Payload;
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

use etr::handshake::process_client_hello;
use etr::protocol::{
    Disconnect, Envelope, ForwardProto, Heartbeat, PacketHeader, Payload, StreamClose, StreamData,
    StreamType,
};
use etr::session::SessionState;
use etr::transport::{ReceivedPacket, decode_data_packet, recv_packet, send_packet};

#[derive(Parser)]
#[command(
    name = "etrs",
    version = "0.2.0",
    about = "Eternal Terminal Server — started per-session by etr via SSH"
)]
struct Cli {
    /// UDP port to bind (0 = random)
    #[arg(short, long, default_value = "0")]
    port: u16,

    /// IP address to bind
    #[arg(short, long, default_value = "[::]")]
    bind: String,

    /// Verbosity: -v session events, -vv cipher details, -vvv packet trace
    #[arg(short = 'v', action = ArgAction::Count)]
    verbose: u8,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let _ = VERBOSITY.set(cli.verbose);

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

    // Bind UDP socket synchronously so we know the actual port before fork.
    let bind_str = format!("{}:{}", cli.bind, cli.port);
    let std_socket = std::net::UdpSocket::bind(&bind_str)?;
    std_socket.set_nonblocking(true)?;
    let actual_port = std_socket.local_addr()?.port();

    // Tell the client which port we bound so it can connect.
    println!("PORT {actual_port}");
    io::stdout().flush()?;

    // Fork: parent exits (SSH session closes cleanly); child runs the session.
    use nix::unistd::{ForkResult, fork, setsid};
    match unsafe { fork() }.map_err(|e| io::Error::other(e.to_string()))? {
        ForkResult::Parent { .. } => return Ok(()),
        ForkResult::Child => {
            setsid().ok();
            detach_stdio()?;
        }
    }

    // Child: build the Tokio runtime and run.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run_session(std_socket, session_id, passkey, term))
}

/// Redirect stdin/stdout to /dev/null and stderr to the session log file.
fn detach_stdio() -> io::Result<()> {
    use nix::unistd::dup2;
    use std::os::unix::io::IntoRawFd;

    let null_fd = std::fs::File::open("/dev/null")
        .map(|f| f.into_raw_fd())
        .unwrap_or(-1);

    let log_path = session_log_path();
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

/// Run one reconnect-aware session until the client cleanly disconnects
/// or the 30-minute reconnect window expires.
async fn run_session(
    std_socket: std::net::UdpSocket,
    session_id: [u8; 16],
    passkey: String,
    term: String,
) -> io::Result<()> {
    let socket = Arc::new(UdpSocket::from_std(std_socket)?);
    vlog!(
        1,
        "[etrs] session {} port={}",
        hex_encode(&session_id),
        socket.local_addr()?.port()
    );

    // --- PTY setup ---
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

    // These must be called before pair.master is moved into the Arc<Mutex<>>.
    let mut pty_reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
    let mut pty_writer = pair.master.take_writer().map_err(io::Error::other)?;
    let master = Arc::new(Mutex::new(pair.master));

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

    // Pointer to the outbound channel for the current connection (None when disconnected).
    let outbound_tx: Arc<std::sync::Mutex<Option<mpsc::Sender<Envelope>>>> =
        Arc::new(std::sync::Mutex::new(None));

    // PTY reader: forwards PTY output into the session state and the outbound channel.
    {
        let outbound_tx = Arc::clone(&outbound_tx);
        let session_state = Arc::clone(&session_state);
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 4096];
            loop {
                match pty_reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        let seq = {
                            let mut s = futures::executor::block_on(session_state.lock());
                            let st = s.stream_mut(0).expect("stream 0 always exists");
                            let seq = st.next_out_seq;
                            st.next_out_seq += 1;
                            st.record_send(seq, data.clone());
                            seq
                        };
                        let tx = outbound_tx.lock().unwrap().clone();
                        if let Some(tx) = tx {
                            let _ = tx.blocking_send(Envelope {
                                payload: Some(Payload::StreamData(StreamData {
                                    stream_id: 0,
                                    seq_num: seq,
                                    data,
                                    ..Default::default()
                                })),
                            });
                        }
                    }
                }
            }
        });
    }

    // Shell-exit watcher: sends Disconnect to the client when the shell exits.
    {
        let outbound_tx = Arc::clone(&outbound_tx);
        let session_id_copy = session_id;
        tokio::task::spawn_blocking(move || {
            let _ = child.wait();
            vlog!(
                1,
                "[etrs] shell exited for {}",
                hex_encode(&session_id_copy)
            );
            if let Some(tx) = outbound_tx.lock().unwrap().clone() {
                let _ = tx.blocking_send(Envelope {
                    payload: Some(Payload::Disconnect(Disconnect {})),
                });
            }
        });
    }

    // Pointer to the inbound channel for the current connection.
    let inbound_tx: Arc<std::sync::Mutex<Option<mpsc::Sender<ReceivedPacket>>>> =
        Arc::new(std::sync::Mutex::new(None));

    // Channel on which the routing task delivers ClientHello packets.
    let (hello_tx, mut hello_rx) = mpsc::channel::<ReceivedPacket>(4);

    // Routing task: reads the UDP socket and routes handshake vs. data packets.
    {
        let inbound_tx = Arc::clone(&inbound_tx);
        let socket = Arc::clone(&socket);
        tokio::spawn(async move {
            loop {
                let pkt = match recv_packet(&socket).await {
                    Ok(Some(p)) => p,
                    Ok(None) => continue,
                    Err(_) => break,
                };
                if pkt.header.session_id != session_id {
                    continue;
                }
                if pkt.header.is_handshake() {
                    let _ = hello_tx.send(pkt).await;
                } else {
                    let tx = inbound_tx.lock().unwrap().clone();
                    if let Some(tx) = tx {
                        let _ = tx.try_send(pkt);
                    }
                }
            }
        });
    }

    // --- Reconnect loop ---
    const RECONNECT_WINDOW: Duration = Duration::from_secs(30 * 60);

    loop {
        let hello_pkt = match tokio::time::timeout(RECONNECT_WINDOW, hello_rx.recv()).await {
            Ok(Some(p)) => p,
            _ => {
                vlog!(1, "[etrs] reconnect window expired, shutting down");
                break;
            }
        };

        let peer = hello_pkt.peer;
        vlog!(
            1,
            "[etrs] ClientHello from {} session={}",
            peer,
            hex_encode(&session_id)
        );

        let server_last = session_state.lock().await.last_received_map();
        let outcome = match process_client_hello(&hello_pkt.payload_bytes, server_last, |_| {
            Some(passkey.clone())
        }) {
            Ok(o) => o,
            Err(e) => {
                vlog!(1, "[etrs] handshake failed from {}: {}", peer, e);
                continue;
            }
        };

        vlog!(
            2,
            "[etrs] handshake complete suite={} peer={}",
            outcome.chosen_suite,
            peer
        );

        let replays = {
            let mut s = session_state.lock().await;
            s.apply_server_acks(&outcome.client_last_received);
            s.collect_replays(&outcome.client_last_received)
        };

        // Send ServerHello.
        let mut buf = Vec::new();
        buf.extend_from_slice(&outcome.response_header.encode());
        buf.extend_from_slice(&outcome.response_payload_bytes);
        socket.send_to(&buf, peer).await?;

        let cipher = Arc::new(outcome.cipher);

        // Create per-connection channels.
        let (ob_tx, mut ob_rx) = mpsc::channel::<Envelope>(1000);
        *outbound_tx.lock().unwrap() = Some(ob_tx.clone());

        let (ib_tx, mut ib_rx) = mpsc::channel::<ReceivedPacket>(256);
        *inbound_tx.lock().unwrap() = Some(ib_tx);

        // Queue replays ahead of live data.
        for (stream_id, packets) in replays {
            for (seq, data) in packets {
                let _ = ob_tx.try_send(Envelope {
                    payload: Some(Payload::StreamData(StreamData {
                        stream_id,
                        seq_num: seq,
                        data,
                        ..Default::default()
                    })),
                });
            }
        }

        // Writer task.
        let socket_w = Arc::clone(&socket);
        let cipher_w = Arc::clone(&cipher);
        let session_w = Arc::clone(&session_state);
        let mut writer = tokio::spawn(async move {
            use prost::Message as _;
            while let Some(env) = ob_rx.recv().await {
                let seq = {
                    let mut s = session_w.lock().await;
                    s.next_packet_seq()
                };
                vlog!(
                    3,
                    "[etrs] → {} seq={} {}b peer={}",
                    payload_type(env.payload.as_ref()),
                    seq,
                    env.encoded_len(),
                    peer
                );
                let header = PacketHeader::new(0, session_id, seq);
                let _ = send_packet(&socket_w, peer, &header, &env, Some(&cipher_w)).await;
            }
        });

        // Per-connection registry of active forward-stream handlers.
        // stream_id → channel for data arriving from the client.
        let fwd_tasks: Arc<
            tokio::sync::Mutex<std::collections::HashMap<u32, mpsc::Sender<StreamData>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        // Reader task — returns true on clean Disconnect, false on timeout/replacement.
        let cipher_r = Arc::clone(&cipher);
        let session_r = Arc::clone(&session_state);
        let pty_tx = pty_in_tx.clone();
        let master_r = Arc::clone(&master);
        let ob_tx_r = ob_tx.clone();
        let fwd_tasks_r = Arc::clone(&fwd_tasks);
        let mut reader = tokio::spawn(async move {
            loop {
                match tokio::time::timeout(Duration::from_secs(15), ib_rx.recv()).await {
                    Ok(Some(pkt)) => {
                        if pkt.header.is_handshake() {
                            return false;
                        }
                        let env = match decode_data_packet(
                            &pkt.payload_bytes,
                            pkt.header.packet_seq,
                            &cipher_r,
                        ) {
                            Ok(e) => e,
                            Err(_) => continue,
                        };
                        vlog!(
                            3,
                            "[etrs] ← {} seq={} {}b peer={}",
                            payload_type(env.payload.as_ref()),
                            pkt.header.packet_seq,
                            pkt.payload_bytes.len(),
                            peer
                        );
                        match env.payload {
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
                            Some(Payload::StreamData(sd)) => {
                                // Route to the forward-stream handler.
                                if let Some(tx) = fwd_tasks_r.lock().await.get(&sd.stream_id) {
                                    let _ = tx.send(sd).await;
                                }
                            }
                            Some(Payload::StreamOpen(so))
                                if so.stream_type == StreamType::PortForward as i32 =>
                            {
                                let (fwd_tx, fwd_rx) = mpsc::channel::<StreamData>(256);
                                fwd_tasks_r.lock().await.insert(so.stream_id, fwd_tx);
                                let ob = ob_tx_r.clone();
                                let ft = Arc::clone(&fwd_tasks_r);
                                match ForwardProto::try_from(so.forward_proto)
                                    .unwrap_or(ForwardProto::Tcp)
                                {
                                    ForwardProto::Tcp => {
                                        tokio::spawn(serve_tcp_forward(
                                            so.stream_id,
                                            so.remote_host,
                                            so.remote_port as u16,
                                            fwd_rx,
                                            ob,
                                            ft,
                                        ));
                                    }
                                    ForwardProto::Udp => {
                                        tokio::spawn(serve_udp_forward(
                                            so.stream_id,
                                            so.remote_host,
                                            so.remote_port as u16,
                                            fwd_rx,
                                            ob,
                                            ft,
                                        ));
                                    }
                                }
                            }
                            Some(Payload::StreamClose(sc)) if sc.stream_id != 0 => {
                                fwd_tasks_r.lock().await.remove(&sc.stream_id);
                            }
                            Some(Payload::TerminalResize(tr)) => {
                                let m = master_r.lock().await;
                                let _ = m.resize(PtySize {
                                    rows: tr.rows as u16,
                                    cols: tr.cols as u16,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                });
                            }
                            Some(Payload::Disconnect(_)) => return true,
                            Some(Payload::Heartbeat(_)) => {}
                            _ => {}
                        }
                    }
                    Ok(None) => return false,
                    Err(_) => {
                        vlog!(
                            1,
                            "[etrs] client {} heartbeat timeout, awaiting reconnect",
                            peer
                        );
                        return false;
                    }
                }
            }
        });

        // Heartbeat task.
        let ob_hb = ob_tx.clone();
        let mut hb = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if ob_hb
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

        let clean_disconnect = tokio::select! {
            r = &mut reader => r.unwrap_or(false),
            _ = &mut writer => false,
            _ = &mut hb    => false,
        };
        writer.abort();
        reader.abort();
        hb.abort();
        *outbound_tx.lock().unwrap() = None;
        *inbound_tx.lock().unwrap() = None;

        if clean_disconnect {
            vlog!(1, "[etrs] client disconnected cleanly, shutting down");
            break;
        }
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

// ── Port-forward helpers (server side) ───────────────────────────────────────

type FwdRegistry =
    Arc<tokio::sync::Mutex<std::collections::HashMap<u32, mpsc::Sender<StreamData>>>>;

/// Connect to `remote_host:remote_port` via TCP and pipe data with the client stream.
async fn serve_tcp_forward(
    stream_id: u32,
    remote_host: String,
    remote_port: u16,
    mut fwd_rx: mpsc::Receiver<StreamData>,
    ob_tx: mpsc::Sender<Envelope>,
    fwd_registry: FwdRegistry,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let addr = format!("{remote_host}:{remote_port}");
    let stream = match TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(e) => {
            vlog!(1, "[etrs] TCP connect to {addr} failed: {e}");
            let _ = ob_tx
                .send(Envelope {
                    payload: Some(Payload::StreamClose(StreamClose {
                        stream_id,
                        error_code: 1,
                    })),
                })
                .await;
            fwd_registry.lock().await.remove(&stream_id);
            return;
        }
    };
    vlog!(2, "[etrs] TCP forward stream {stream_id} → {addr}");

    let (mut tcp_rx, mut tcp_tx) = stream.into_split();

    // Remote TCP → StreamData to client.
    let ob2 = ob_tx.clone();
    let mut reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        let mut seq: u64 = 1;
        loop {
            match tcp_rx.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let sn = seq;
                    seq += 1;
                    if ob2
                        .send(Envelope {
                            payload: Some(Payload::StreamData(StreamData {
                                stream_id,
                                seq_num: sn,
                                data: buf[..n].to_vec(),
                                ..Default::default()
                            })),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = ob2
            .send(Envelope {
                payload: Some(Payload::StreamClose(StreamClose {
                    stream_id,
                    error_code: 0,
                })),
            })
            .await;
    });

    // StreamData from client → remote TCP.
    let mut writer = tokio::spawn(async move {
        while let Some(sd) = fwd_rx.recv().await {
            if tcp_tx.write_all(&sd.data).await.is_err() {
                break;
            }
        }
        let _ = tcp_tx.shutdown().await;
    });

    tokio::select! {
        _ = &mut reader => {}
        _ = &mut writer => {}
    }
    reader.abort();
    writer.abort();
    fwd_registry.lock().await.remove(&stream_id);
    vlog!(2, "[etrs] TCP forward stream {stream_id} closed");
}

/// Bind a UDP socket and forward datagrams to `remote_host:remote_port`.
///
/// All client datagrams go to the same remote target (shared-socket model).
/// The `peer_addr`/`peer_port` embedded in each client `StreamData` is echoed
/// back in the reply so the client can route it to the right local sender.
/// Last-sender routing is used when a reply arrives and multiple local senders
/// have been seen — sufficient for single-sender use cases and sequential
/// request/response protocols (DNS, STUN, etc.).
async fn serve_udp_forward(
    stream_id: u32,
    remote_host: String,
    remote_port: u16,
    mut fwd_rx: mpsc::Receiver<StreamData>,
    ob_tx: mpsc::Sender<Envelope>,
    fwd_registry: FwdRegistry,
) {
    use tokio::net::UdpSocket as TUdp;

    let remote_addr = format!("{remote_host}:{remote_port}");
    let socket = match TUdp::bind("0.0.0.0:0").await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            vlog!(1, "[etrs] UDP bind failed for stream {stream_id}: {e}");
            let _ = ob_tx
                .send(Envelope {
                    payload: Some(Payload::StreamClose(StreamClose {
                        stream_id,
                        error_code: 1,
                    })),
                })
                .await;
            fwd_registry.lock().await.remove(&stream_id);
            return;
        }
    };
    vlog!(2, "[etrs] UDP forward stream {stream_id} → {remote_addr}");

    // Track the most-recent local sender so replies can be routed back.
    let last_peer: Arc<std::sync::Mutex<Option<(String, u32)>>> =
        Arc::new(std::sync::Mutex::new(None));
    let last_peer2 = Arc::clone(&last_peer);

    let socket2 = Arc::clone(&socket);
    let ob2 = ob_tx.clone();

    // Incoming replies from the remote → StreamData to client.
    let mut reply_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        let mut seq: u64 = 1;
        #[allow(clippy::while_let_loop)] // loop has a `continue` branch, not just break
        loop {
            match socket2.recv_from(&mut buf).await {
                Ok((n, _src)) => {
                    let (peer_addr, peer_port) = {
                        let g = last_peer2.lock().unwrap();
                        match g.as_ref() {
                            Some((a, p)) => (a.clone(), *p),
                            None => continue, // no sender yet; discard
                        }
                    };
                    let sn = seq;
                    seq += 1;
                    if ob2
                        .send(Envelope {
                            payload: Some(Payload::StreamData(StreamData {
                                stream_id,
                                seq_num: sn,
                                data: buf[..n].to_vec(),
                                peer_addr,
                                peer_port,
                            })),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Client StreamData → remote UDP.
    let mut send_task = tokio::spawn(async move {
        while let Some(sd) = fwd_rx.recv().await {
            // Update last-seen sender for reply routing.
            if !sd.peer_addr.is_empty() {
                *last_peer.lock().unwrap() = Some((sd.peer_addr.clone(), sd.peer_port));
            }
            let _ = socket.send_to(&sd.data, &remote_addr).await;
        }
    });

    tokio::select! {
        _ = &mut reply_task => {}
        _ = &mut send_task  => {}
    }
    reply_task.abort();
    send_task.abort();
    fwd_registry.lock().await.remove(&stream_id);
    vlog!(2, "[etrs] UDP forward stream {stream_id} closed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

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
}
