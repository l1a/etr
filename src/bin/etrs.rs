use clap::{Parser, Subcommand};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::HashMap;
use std::io::IsTerminal;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};

use etr::crypto::{SessionCipher, generate_nonce};
use etr::protocol::Packet;
use etr::session::{SessionState, read_frame, recv_encrypted, send_encrypted, write_frame};

#[derive(Parser)]
#[command(
    name = "etrs",
    version = "0.1.1",
    about = "Eternal Terminal Server Daemon in Rust"
)]
struct Cli {
    #[arg(short, long, default_value = "2022")]
    port: u16,

    #[arg(short, long, default_value = "0.0.0.0")]
    bind: String,

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

type ActiveChannel = Option<(mpsc::Sender<Packet>, u64)>;

struct ActiveSession {
    client_id: String,
    session_state: Arc<Mutex<SessionState>>,
    pty_write_tx: mpsc::Sender<Vec<u8>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    // Active channel to send packets to the TCP writer task (sender and connection ID)
    tcp_tx: Arc<std::sync::Mutex<ActiveChannel>>,
    // Next connection ID to assign
    next_conn_id: Arc<Mutex<u64>>,
}

type SessionMap = Arc<std::sync::Mutex<HashMap<String, Arc<ActiveSession>>>>;

#[tokio::main]
async fn main() -> io::Result<()> {
    let cli = Cli::parse();

    let cmd = cli.command.unwrap_or_else(|| {
        if !io::stdin().is_terminal() {
            Commands::Register
        } else {
            Commands::Daemon
        }
    });

    match cmd {
        Commands::Daemon => run_daemon(cli.bind, cli.port, cli.socket).await,
        Commands::Register => run_register(cli.socket).await,
    }
}

async fn run_register(socket_path: String) -> io::Result<()> {
    // Read the handshake string from stdin: CLIENT_ID/PASSKEY/TERM
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();

    let parts: Vec<&str> = input.split('/').collect();
    if parts.len() < 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Handshake input must be formatted as CLIENT_ID/PASSKEY/TERM",
        ));
    }

    let client_id = parts[0].to_string();
    let passkey = parts[1].to_string();
    let term = parts[2].to_string();

    // Connect to the daemon's Unix socket to register
    let mut stream = UnixStream::connect(&socket_path).await?;

    // Simple protocol to register session: write CLIENT_ID/PASSKEY/TERM
    let reg_msg = format!("{}/{}/{}\n", client_id, passkey, term);
    stream.write_all(reg_msg.as_bytes()).await?;

    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    if response.trim() == "OK" {
        println!("Session registered successfully.");
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "Registration failed: {}",
            response
        )))
    }
}

async fn run_daemon(bind_addr: String, port: u16, socket_path: String) -> io::Result<()> {
    println!("Starting etrs daemon...");

    let sessions: SessionMap = Arc::new(std::sync::Mutex::new(HashMap::new()));

    // Clean up old socket if it exists
    let _ = std::fs::remove_file(&socket_path);

    // Bind Unix domain socket for session registration
    let unix_listener = UnixListener::bind(&socket_path)?;
    let sessions_clone = Arc::clone(&sessions);
    tokio::spawn(async move {
        while let Ok((stream, _)) = unix_listener.accept().await {
            let sessions_inner = Arc::clone(&sessions_clone);
            tokio::spawn(async move {
                if let Err(e) = handle_registration(stream, sessions_inner).await {
                    eprintln!("Error handling session registration: {:?}", e);
                }
            });
        }
    });

    // Bind TCP listener for incoming client connections
    let addr: SocketAddr = format!("{}:{}", bind_addr, port)
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let tcp_listener = TcpListener::bind(&addr).await?;
    println!("Listening on TCP address: {}", addr);

    while let Ok((stream, peer_addr)) = tcp_listener.accept().await {
        let sessions_inner = Arc::clone(&sessions);
        tokio::spawn(async move {
            println!("Connection received from {}", peer_addr);
            if let Err(e) = handle_client_tcp(stream, sessions_inner).await {
                eprintln!("Error handling client TCP connection: {:?}", e);
            }
        });
    }

    Ok(())
}

async fn handle_registration(mut stream: UnixStream, sessions: SessionMap) -> io::Result<()> {
    let mut buf = vec![0u8; 1024];
    let n = stream.read(&mut buf).await?;
    let msg = String::from_utf8_lossy(&buf[0..n]);
    let parts: Vec<&str> = msg.trim().split('/').collect();
    if parts.len() < 3 {
        stream
            .write_all(b"ERROR: Invalid registration format")
            .await?;
        return Ok(());
    }

    let client_id = parts[0].to_string();
    let passkey = parts[1].to_string();
    let term = parts[2].to_string();

    println!("Registering session client_id={} term={}", client_id, term);

    // Initialize PTY
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
    let mut cmd = CommandBuilder::new(shell);
    cmd.env("TERM", term);

    let mut child = pair.slave.spawn_command(cmd).map_err(io::Error::other)?;

    let master = pair.master;
    let mut pty_reader = master.try_clone_reader().map_err(io::Error::other)?;
    let mut pty_writer = master.take_writer().map_err(io::Error::other)?;

    // Channel for writing to PTY
    let (pty_write_tx, mut pty_write_rx) = mpsc::channel::<Vec<u8>>(1000);

    // Task to write to PTY
    tokio::task::spawn_blocking(move || {
        while let Some(data) = pty_write_rx.blocking_recv() {
            if pty_writer.write_all(&data).is_err() {
                break;
            }
            let _ = pty_writer.flush();
        }
    });

    let master_shared = Arc::new(Mutex::new(master));
    let session_state = Arc::new(Mutex::new(SessionState::new(client_id.clone(), passkey)));
    let tcp_tx = Arc::new(std::sync::Mutex::new(None));
    let next_conn_id = Arc::new(Mutex::new(1));

    let active_session = Arc::new(ActiveSession {
        client_id: client_id.clone(),
        session_state: Arc::clone(&session_state),
        pty_write_tx,
        master: Arc::clone(&master_shared),
        tcp_tx: Arc::clone(&tcp_tx),
        next_conn_id: Arc::clone(&next_conn_id),
    });

    // Spawn task to wait on child process exit
    let tcp_tx_child = Arc::clone(&tcp_tx);
    let sessions_cleanup_child = Arc::clone(&sessions);
    let client_id_cleanup_child = client_id.clone();
    tokio::task::spawn_blocking(move || {
        let _ = child.wait();
        println!("Shell child process exited. Cleaning up session.");

        // Remove session from map
        let mut map = sessions_cleanup_child.lock().unwrap();
        if map.remove(&client_id_cleanup_child).is_some() {
            println!(
                "Session client_id={} removed on child exit.",
                client_id_cleanup_child
            );
        }
        drop(map);

        // Signal the client to disconnect cleanly
        let mut tcp_tx_guard = tcp_tx_child.lock().unwrap();
        if let Some((tx, _)) = &*tcp_tx_guard {
            let _ = tx.blocking_send(Packet::Disconnect);
        }
        *tcp_tx_guard = None;
    });

    // Spawn a task to read from PTY master and forward it to client
    let session_state_reader = Arc::clone(&session_state);
    let tcp_tx_reader = Arc::clone(&tcp_tx);
    let sessions_cleanup = Arc::clone(&sessions);
    let client_id_cleanup = client_id.clone();
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = pty_reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            let payload = buf[0..n].to_vec();

            // Lock and get current sequence number
            let mut state = futures::executor::block_on(session_state_reader.lock());
            let seq = state.next_out_seq;
            state.next_out_seq += 1;

            // Record in history
            state.record_send(seq, payload.clone());
            drop(state);

            // Construct packet
            let packet = Packet::TerminalData {
                seq_num: seq,
                data: payload,
            };

            // If we have an active TCP connection, send it
            let tcp_tx_guard = tcp_tx_reader.lock().unwrap();
            if let Some((tx, _)) = &*tcp_tx_guard {
                let _ = tx.blocking_send(packet);
            }
            drop(tcp_tx_guard);
        }
        println!("PTY reader exited. Cleaning up session.");

        // Remove session from map when PTY exits (logout)
        let mut map = sessions_cleanup.lock().unwrap();
        if map.remove(&client_id_cleanup).is_some() {
            println!(
                "Session client_id={} removed on PTY reader exit.",
                client_id_cleanup
            );
        }
        drop(map);

        // Also signal the client to disconnect cleanly
        let mut tcp_tx_guard = tcp_tx_reader.lock().unwrap();
        if let Some((tx, _)) = &*tcp_tx_guard {
            let _ = tx.blocking_send(Packet::Disconnect);
        }
        *tcp_tx_guard = None;
    });

    // Add session to map
    sessions.lock().unwrap().insert(client_id, active_session);

    stream.write_all(b"OK").await?;
    Ok(())
}

async fn handle_client_tcp(mut stream: TcpStream, sessions: SessionMap) -> io::Result<()> {
    // 1. Read ConnectRequest from client
    let request_bytes = read_frame(&mut stream).await?;
    let packet: Packet = bincode::deserialize(&request_bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let (client_id, client_nonce) = match packet {
        Packet::ConnectRequest {
            client_id,
            client_nonce,
        } => (client_id, client_nonce),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected ConnectRequest",
            ));
        }
    };

    // 2. Lookup session
    let session = {
        let guard = sessions.lock().unwrap();
        guard.get(&client_id).cloned()
    };

    let session = match session {
        Some(s) => s,
        None => {
            eprintln!("Session not found: {}", client_id);
            // Send Disconnect packet to client before closing to let them know the session is dead
            let response = Packet::Disconnect;
            if let Ok(response_bytes) = bincode::serialize(&response) {
                let _ = write_frame(&mut stream, &response_bytes).await;
            }
            return Ok(());
        }
    };

    // 3. Generate server nonce and send ConnectResponse
    let server_nonce = generate_nonce();
    let response = Packet::ConnectResponse { server_nonce };
    let response_bytes =
        bincode::serialize(&response).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_frame(&mut stream, &response_bytes).await?;

    // 4. Instantiate SessionCipher
    let state_guard = session.session_state.lock().await;
    let cipher = Arc::new(SessionCipher::new(
        &state_guard.passkey,
        &client_nonce,
        &server_nonce,
    ));
    drop(state_guard);

    // 5. Perform Encrypted Auth Handshake
    // The client sends Auth { mac }
    let client_auth = recv_encrypted(&mut stream, &cipher, 0).await?;
    if !matches!(client_auth, Packet::Auth { .. }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Handshake Auth failed",
        ));
    }

    // Server sends Auth response back
    send_encrypted(&mut stream, &cipher, 0, &Packet::Auth { mac: [0u8; 32] }).await?;
    println!("Session authenticated successfully: {}", client_id);

    // 6. Handle Reconnection Sync Handshake
    // Client sends SyncRequest
    let sync_req = recv_encrypted(&mut stream, &cipher, 0).await?;
    let client_last_received = match sync_req {
        Packet::SyncRequest { last_received_seq } => last_received_seq,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected SyncRequest",
            ));
        }
    };

    let mut state = session.session_state.lock().await;
    let server_last_received = state.next_in_seq - 1;

    // Send SyncResponse
    send_encrypted(
        &mut stream,
        &cipher,
        0,
        &Packet::SyncResponse {
            last_received_seq: server_last_received,
        },
    )
    .await?;

    // Clean up acknowledge logs
    state.acknowledge_up_to(client_last_received);

    // 7. Replay any missed packets
    let replays = state.get_replay_packets(client_last_received);
    drop(state);

    // 8. Set up channel for client TCP writing and assign unique connection ID
    let conn_id = {
        let mut guard = session.next_conn_id.lock().await;
        let id = *guard;
        *guard += 1;
        id
    };

    let (tcp_send_tx, mut tcp_send_rx) = mpsc::channel::<Packet>(1000);

    // Queue catch-up packets to the sender channel to be encrypted with transport sequence numbers
    for (seq, data) in replays {
        let packet = Packet::TerminalData { seq_num: seq, data };
        let _ = tcp_send_tx.send(packet).await;
    }

    // Register the new TCP writer channel in the active session along with the connection ID
    {
        let mut active_tx = session.tcp_tx.lock().unwrap();
        *active_tx = Some((tcp_send_tx, conn_id));
    }

    // Split TCP stream
    let (mut reader, mut writer) = stream.into_split();

    // Task to write data to client TCP socket
    let cipher_clone = Arc::clone(&cipher);
    let mut writer_task = tokio::spawn(async move {
        let mut transport_out_seq = 1;
        while let Some(packet) = tcp_send_rx.recv().await {
            if let Err(e) =
                send_encrypted_writer(&mut writer, &cipher_clone, transport_out_seq, &packet).await
            {
                eprintln!("TCP writer task error: {:?}", e);
                break;
            }
            transport_out_seq += 1;
        }
    });

    // Task to read data from client TCP socket with a 15-second idle timeout
    let session_state_writer = Arc::clone(&session.session_state);
    let pty_write_tx = session.pty_write_tx.clone();
    let master_clone = Arc::clone(&session.master);
    let mut reader_task = tokio::spawn(async move {
        let mut expected_seq = {
            let guard = session_state_writer.lock().await;
            guard.next_in_seq
        };

        let mut transport_in_seq = 1;
        loop {
            // Read length-framed packet from socket with a 15-second timeout
            let encrypted = match tokio::time::timeout(
                std::time::Duration::from_secs(15),
                read_frame_reader(&mut reader),
            )
            .await
            {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(_)) => break, // Socket disconnected
                Err(_) => {
                    eprintln!("Client connection timed out due to inactivity.");
                    break;
                }
            };

            let decrypted = match cipher.decrypt(transport_in_seq, &encrypted) {
                Ok(bytes) => bytes,
                Err(e) => {
                    eprintln!(
                        "TCP reader decryption failed at transport_seq={}: {:?}",
                        transport_in_seq, e
                    );
                    break;
                }
            };
            transport_in_seq += 1;

            let packet: Packet = match bincode::deserialize(&decrypted) {
                Ok(p) => p,
                Err(_) => break,
            };

            match packet {
                Packet::TerminalData { seq_num, data } => {
                    if seq_num == expected_seq {
                        // Forward keypresses to PTY
                        let _ = pty_write_tx.send(data).await;
                        expected_seq += 1;

                        let mut guard = session_state_writer.lock().await;
                        guard.next_in_seq = expected_seq;
                    }
                }
                Packet::TerminalResize { rows, cols } => {
                    println!("Resize event: rows={} cols={}", rows, cols);
                    let master_guard = master_clone.lock().await;
                    let _ = master_guard.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                Packet::Heartbeat => {
                    // Handled implicitly by connection remaining open
                }
                Packet::Disconnect => {
                    println!("Client disconnected cleanly.");
                    break;
                }
                _ => {}
            }
        }
    });

    // Task to periodically send heartbeats every 5 seconds to prevent idle timeout
    let tcp_send_tx_heartbeat = session.tcp_tx.clone();
    let mut heartbeat_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let tx_opt = {
                let tx_guard = tcp_send_tx_heartbeat.lock().unwrap();
                if let Some((tx, id)) = &*tx_guard {
                    if *id == conn_id {
                        Some(tx.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            if let Some(tx) = tx_opt {
                if tx.send(Packet::Heartbeat).await.is_err() {
                    break;
                }
            } else {
                break;
            }
        }
    });

    // Wait for reader, writer or heartbeat task to finish (indicates connection drop)
    tokio::select! {
        _ = &mut writer_task => {},
        _ = &mut reader_task => {},
        _ = &mut heartbeat_task => {},
    }

    // Abort all spawned tasks to clean up socket resources cleanly and prevent task leakage
    writer_task.abort();
    reader_task.abort();
    heartbeat_task.abort();

    {
        let mut active_tx = session.tcp_tx.lock().unwrap();
        if let Some((_, active_id)) = &*active_tx {
            if *active_id == conn_id {
                *active_tx = None;
                println!(
                    "Client connection lost for client_id={}, cleaning up active channel.",
                    session.client_id
                );
            } else {
                println!(
                    "Client connection for client_id={} was replaced by a newer connection (conn_id={}), skipping cleanup.",
                    session.client_id, active_id
                );
            }
        }
    }

    Ok(())
}

// Low-level helper functions for split socket operations

async fn send_encrypted_writer(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    cipher: &SessionCipher,
    seq_num: u64,
    packet: &Packet,
) -> io::Result<()> {
    let raw_bytes =
        bincode::serialize(packet).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let encrypted = cipher
        .encrypt(seq_num, &raw_bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{:?}", e)))?;
    let len = encrypted.len() as u32;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&encrypted).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_frame_reader(reader: &mut tokio::net::tcp::OwnedReadHalf) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 10 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}
