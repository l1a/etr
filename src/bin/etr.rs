use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use clap_complete_nushell::Nushell;
use crossterm::event::{self, Event};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, Write};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};

use etr::crypto::{SessionCipher, generate_nonce};
use etr::protocol::Packet;
use etr::session::{SessionState, read_frame, recv_encrypted, send_encrypted, write_frame};

#[derive(Parser, Debug)]
#[command(
    name = "etr",
    version = "0.1.1",
    about = "Eternal Terminal Client in Rust"
)]
struct Cli {
    /// Remote host target (e.g. user@host or host)
    target: Option<String>,

    /// Remote TCP port of etr server
    #[arg(short, long, default_value = "2022")]
    port: u16,

    /// SSH port for initial authentication
    #[arg(short = 's', long, default_value = "22")]
    ssh_port: u16,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug, Clone)]
enum Commands {
    /// Generate shell completions for the specified shell
    Completions {
        /// The shell to generate completions for
        #[arg(value_enum)]
        shell: ShellChoice,
    },
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

    if let Some(command) = cli.command {
        match command {
            Commands::Completions { shell } => {
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
                    ShellChoice::PowerShell => clap_complete::generate(
                        Shell::PowerShell,
                        &mut cmd,
                        "etr",
                        &mut io::stdout(),
                    ),
                    ShellChoice::Zsh => {
                        clap_complete::generate(Shell::Zsh, &mut cmd, "etr", &mut io::stdout())
                    }
                    ShellChoice::Nushell => {
                        clap_complete::generate(Nushell, &mut cmd, "etr", &mut io::stdout())
                    }
                }
                return Ok(());
            }
        }
    }

    let target = match cli.target {
        Some(t) => t,
        None => {
            let mut cmd = Cli::command();
            let _ = cmd.print_help();
            return Ok(());
        }
    };

    // 1. Generate credentials
    let client_id = generate_id();
    let passkey = generate_passkey();
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());

    println!("Connecting to {} via SSH to bootstrap session...", target);

    // 2. Perform SSH handshake to register session
    bootstrap_ssh(&target, cli.ssh_port, &client_id, &passkey, &term)?;

    // 3. Setup persistent session state
    let session_state = Arc::new(Mutex::new(SessionState::new(client_id.clone(), passkey)));

    // 4. Run persistent connection loop
    if let Err(e) = run_connection_loop(target, cli.port, session_state).await {
        eprintln!("Session connection loop terminated: {:?}", e);
    }

    Ok(())
}

fn generate_id() -> String {
    use rand::Rng;
    let s: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(16)
        .map(char::from)
        .collect();
    s
}

fn generate_passkey() -> String {
    use rand::Rng;
    let s: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    s
}

fn bootstrap_ssh(
    target: &str,
    ssh_port: u16,
    client_id: &str,
    passkey: &str,
    term: &str,
) -> io::Result<()> {
    // Construct the remote command to run etrs
    let remote_command = "etrs register";

    // Spawn SSH process
    let mut child = Command::new("ssh")
        .arg("-p")
        .arg(ssh_port.to_string())
        .arg(target)
        .arg(remote_command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()?;

    // Write the registration handshake CLIENT_ID/PASSKEY/TERM to SSH stdin
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Failed to open SSH stdin pipe"))?;

    let handshake = format!("{}/{}/{}\n", client_id, passkey, term);
    stdin.write_all(handshake.as_bytes())?;
    stdin.flush()?;
    drop(stdin); // Close stdin to signal EOF to the remote process

    // Wait for SSH to complete
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let err_msg = String::from_utf8_lossy(&output.stdout);
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("SSH bootstrap failed: {}", err_msg.trim()),
        ));
    }

    Ok(())
}

async fn run_connection_loop(
    target: String,
    port: u16,
    session_state: Arc<Mutex<SessionState>>,
) -> io::Result<()> {
    // Strip user prefix to get just host for TCP connection
    let host = if let Some(idx) = target.find('@') {
        &target[idx + 1..]
    } else {
        &target
    };
    let addr = format!("{}:{}", host, port);

    let mut is_first_connect = true;

    loop {
        if !is_first_connect {
            println!("\r\n[etr] Connection lost. Reconnecting to {}...", addr);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        is_first_connect = false;

        let mut stream = match TcpStream::connect(&addr).await {
            Ok(s) => s,
            Err(_) => continue,
        };

        // Complete handshake & auth
        let (cipher, replays) = match perform_handshake(&mut stream, &session_state).await {
            Ok(res) => res,
            Err(e) => {
                eprintln!("\r\n[etr] Handshake failed: {:?}", e);
                continue;
            }
        };

        println!("\r\n[etr] Connected. Session active.");

        // Enable terminal raw mode
        enable_raw_mode().unwrap();

        // Run session loops
        let run_result = run_session(stream, cipher, &session_state, replays).await;

        // Restore terminal state
        let _ = disable_raw_mode();

        if let Err(e) = run_result {
            eprintln!("\r\n[etr] Session connection dropped: {:?}", e);
        }
    }
}

async fn perform_handshake(
    stream: &mut TcpStream,
    session_state: &Arc<Mutex<SessionState>>,
) -> io::Result<(Arc<SessionCipher>, Vec<(u64, Vec<u8>)>)> {
    let state_guard = session_state.lock().await;
    let client_id = state_guard.client_id.clone();
    let passkey = state_guard.passkey.clone();
    let client_last_received = state_guard.next_in_seq - 1;
    drop(state_guard);

    // 1. Send ConnectRequest
    let client_nonce = generate_nonce();
    let request = Packet::ConnectRequest {
        client_id,
        client_nonce,
    };
    let request_bytes =
        bincode::serialize(&request).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_frame(stream, &request_bytes).await?;

    // 2. Read ConnectResponse
    let response_bytes = read_frame(stream).await?;
    let response: Packet = bincode::deserialize(&response_bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let server_nonce = match response {
        Packet::ConnectResponse { server_nonce } => server_nonce,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected ConnectResponse",
            ));
        }
    };

    // 3. Derive Cipher
    let cipher = Arc::new(SessionCipher::new(&passkey, &client_nonce, &server_nonce));

    // 4. Send encrypted Auth
    send_encrypted(stream, &cipher, 0, &Packet::Auth { mac: [0u8; 32] }).await?;

    // 5. Read encrypted Auth response
    let server_auth = recv_encrypted(stream, &cipher, 0).await?;
    if !matches!(server_auth, Packet::Auth { .. }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Handshake Server Auth failed",
        ));
    }

    // 6. Send SyncRequest
    send_encrypted(
        stream,
        &cipher,
        0,
        &Packet::SyncRequest {
            last_received_seq: client_last_received,
        },
    )
    .await?;

    // 7. Read SyncResponse
    let sync_res = recv_encrypted(stream, &cipher, 0).await?;
    let server_last_received = match sync_res {
        Packet::SyncResponse { last_received_seq } => last_received_seq,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected SyncResponse",
            ));
        }
    };

    // 8. Get replay packets
    let mut state = session_state.lock().await;
    state.acknowledge_up_to(server_last_received);
    let replays = state.get_replay_packets(server_last_received);

    Ok((cipher, replays))
}

async fn run_session(
    stream: TcpStream,
    cipher: Arc<SessionCipher>,
    session_state: &Arc<Mutex<SessionState>>,
    replays: Vec<(u64, Vec<u8>)>,
) -> io::Result<()> {
    // Send catch-up packets
    let mut stream = stream;
    for (seq, data) in replays {
        let packet = Packet::TerminalData { seq_num: seq, data };
        send_encrypted(&mut stream, &cipher, seq, &packet).await?;
    }

    let (tcp_send_tx, mut tcp_send_rx) = mpsc::channel::<Packet>(1000);

    let (mut reader, mut writer) = stream.into_split();

    // Task to write data to server socket
    let cipher_clone = Arc::clone(&cipher);
    let writer_task = tokio::spawn(async move {
        while let Some(packet) = tcp_send_rx.recv().await {
            let seq = match &packet {
                Packet::TerminalData { seq_num, .. } => *seq_num,
                _ => 0,
            };
            if let Err(_) = send_encrypted_writer(&mut writer, &cipher_clone, seq, &packet).await {
                break;
            }
        }
    });

    // Task to read data from local stdin and queue it to write
    let session_state_stdin = Arc::clone(session_state);
    let tcp_send_tx_stdin = tcp_send_tx.clone();
    let stdin_task = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 1024];

        loop {
            let n = match stdin.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            let payload = buf[0..n].to_vec();

            let mut state = session_state_stdin.lock().await;
            let seq = state.next_out_seq;
            state.next_out_seq += 1;
            state.record_send(seq, payload.clone());
            drop(state);

            let packet = Packet::TerminalData {
                seq_num: seq,
                data: payload,
            };

            if tcp_send_tx_stdin.send(packet).await.is_err() {
                break;
            }
        }
    });

    // Task to read data from server TCP socket and write to local stdout
    let session_state_reader = Arc::clone(session_state);
    let reader_task = tokio::spawn(async move {
        let mut expected_seq = {
            let guard = session_state_reader.lock().await;
            guard.next_in_seq
        };

        let mut stdout = io::stdout();

        loop {
            let encrypted = match read_frame_reader(&mut reader).await {
                Ok(bytes) => bytes,
                Err(_) => break,
            };

            let decrypted = match cipher.decrypt(expected_seq, &encrypted) {
                Ok(bytes) => bytes,
                Err(e) => {
                    eprintln!(
                        "Decryption failed on client at seq={}: {:?}",
                        expected_seq, e
                    );
                    break;
                }
            };

            let packet: Packet = match bincode::deserialize(&decrypted) {
                Ok(p) => p,
                Err(_) => break,
            };

            match packet {
                Packet::TerminalData { seq_num, data } => {
                    if seq_num == expected_seq {
                        let _ = stdout.write_all(&data);
                        let _ = stdout.flush();
                        expected_seq += 1;

                        let mut guard = session_state_reader.lock().await;
                        guard.next_in_seq = expected_seq;
                    }
                }
                _ => {}
            }
        }
    });

    // Task to poll terminal resize events
    let tcp_send_tx_resize = tcp_send_tx.clone();
    let resize_task = tokio::spawn(async move {
        loop {
            if let Ok(Ok(true)) =
                tokio::task::spawn_blocking(|| event::poll(Duration::from_millis(500))).await
            {
                if let Ok(Event::Resize(cols, rows)) = event::read() {
                    let packet = Packet::TerminalResize { rows, cols };
                    if tcp_send_tx_resize.send(packet).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Wait for critical stream tasks to complete
    tokio::select! {
        _ = writer_task => {},
        _ = stdin_task => {},
        _ = reader_task => {},
        _ = resize_task => {},
    }

    Ok(())
}

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
