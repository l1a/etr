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
use etr::forward::{ForwardSpec, X11Display, get_xauth_cookie};
use etr::protocol::{
    Envelope, ForwardProto, Heartbeat, Payload, SessionOpen, StreamOpen, TerminalResize,
    UdpDatagram,
};
use etr::quic::{self, TAG_CONTROL, TAG_FORWARD, TAG_PTY};
use etr::session::SessionState;

static LOG_FILE: std::sync::OnceLock<std::sync::Mutex<std::fs::File>> = std::sync::OnceLock::new();
static IN_RAW_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Escape character for the client: `~` (0x7E), SSH-style.  Type it at the
/// start of a line followed by `.` to force-disconnect.  The line-start guard
/// prevents false triggers from `~` in shell paths or git refs.
const ESCAPE_CHAR: u8 = b'~';

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
    version = env!("CARGO_PKG_VERSION"),
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

    /// Remote port forwarding (repeatable): remote_port:local_host:local_port[/tcp|/udp]
    /// Works like ssh -R. Default protocol: tcp.
    /// Example: -R 8080:localhost:80  -R 5353:127.0.0.1:53/udp
    #[arg(short = 'R', value_name = "SPEC")]
    reverse_forward: Vec<String>,

    /// Gateway ports: allow remote hosts to connect to local forwarded ports.
    /// Works like ssh -g. Automatically binds local forwarded ports to all interfaces (0.0.0.0 and ::).
    #[arg(short = 'g', long)]
    gateway_ports: bool,

    /// Path to the client log file (default: $XDG_STATE_HOME/etr/etr.log)
    #[arg(long, value_name = "PATH")]
    log_path: Option<std::path::PathBuf>,

    /// Path to the server log file on the remote host (default: $XDG_STATE_HOME/etr/etrs.log)
    #[arg(long, value_name = "PATH")]
    server_log_path: Option<String>,

    /// Set or forward environment variables to the remote shell (repeatable).
    /// "KEY=VALUE" sets the variable; "KEY" forwards it from the local environment.
    /// Example: --env ZELLIJ_AUTO_START=false --env EDITOR
    #[arg(long = "env", value_name = "KEY[=VALUE]")]
    env: Vec<String>,

    /// Enable X11 forwarding
    #[arg(short = 'X')]
    x11: bool,

    /// Enable trusted X11 forwarding (treated same as -X)
    #[arg(short = 'Y')]
    x11_trusted: bool,

    /// Generate shell completions for the specified shell
    #[arg(long, value_enum, value_name = "SHELL")]
    completions: Option<ShellChoice>,

    /// Remote command to run instead of an interactive shell.
    /// Multiple words are joined with spaces and passed to `sh -c`.
    /// Example: etr host 'distrobox -- btop'
    /// Example: etr host ls -la /tmp
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,

    /// Print a fully-commented default config to stdout
    #[arg(long, help_heading = "Configuration")]
    generate_config: bool,

    /// Write the default config to PATH (default: ~/.config/etr/config.toml).
    /// Creates parent directories as needed. Overwrites any existing file.
    #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "",
          help_heading = "Configuration")]
    write_config: Option<String>,

    /// Add any missing config options (as comments) to the existing config file.
    /// Safe to re-run: already-present keys (active or commented) are never duplicated.
    #[arg(long, help_heading = "Configuration")]
    merge_config: bool,
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
    let cfg = Config::load();

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

    if cli.generate_config {
        print!("{}", etr::config::DEFAULT_CONFIG);
        return Ok(());
    }

    if let Some(path_str) = &cli.write_config {
        let path = if path_str.is_empty() {
            etr::config::config_path()
        } else {
            std::path::PathBuf::from(path_str)
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, etr::config::DEFAULT_CONFIG)?;
        println!("[etr] Wrote default config to {}", path.display());
        return Ok(());
    }

    if cli.merge_config {
        let path = etr::config::config_path();
        if path.exists() {
            let existing = std::fs::read_to_string(&path)?;
            let (new_content, additions) = etr::config::merge_defaults(&existing);
            if additions.is_empty() {
                println!("[etr] Config already contains all known options.");
            } else {
                std::fs::write(&path, &new_content)?;
                println!(
                    "[etr] Added missing options ({}) to {}",
                    additions.join(", "),
                    path.display()
                );
            }
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, etr::config::DEFAULT_CONFIG)?;
            println!("[etr] Created config at {}", path.display());
        }
        return Ok(());
    }

    if cli.verbose > 0 && io::stdin().is_terminal() {
        let log_path = cli
            .log_path
            .clone()
            .or_else(|| cfg.client.log_path.as_ref().map(std::path::PathBuf::from))
            .unwrap_or_else(client_log_path);
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

    let ssh_port = cli
        .ssh_port
        .unwrap_or_else(|| cfg.client.ssh_port.unwrap_or(22));
    let server_path = cli
        .server_path
        .or(cfg.client.server_path)
        .unwrap_or_else(|| "etrs".to_string());

    let forwards = if !cli.forward.is_empty() {
        &cli.forward
    } else if let Some(ref list) = cfg.client.forward {
        list
    } else {
        &cli.forward
    };
    let mut forward_specs: Vec<ForwardSpec> = Vec::new();
    for s in forwards {
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

    let reverse_forwards = if !cli.reverse_forward.is_empty() {
        &cli.reverse_forward
    } else if let Some(ref list) = cfg.client.reverse_forward {
        list
    } else {
        &cli.reverse_forward
    };
    let mut reverse_forward_specs: Vec<String> = Vec::new();
    for s in reverse_forwards {
        match ForwardSpec::parse(s) {
            Ok(spec) => {
                vlog!(cli.verbose, 1, "[etr] Reverse forwarding: {spec}");
                reverse_forward_specs.push(s.clone());
            }
            Err(e) => {
                eprintln!("[etr] error: {e}");
                return Ok(());
            }
        }
    }

    let gateway_ports = if cli.gateway_ports {
        true
    } else {
        cfg.client.gateway_ports.unwrap_or(false)
    };

    // Merge --env flags with [client] env from config, resolving bare KEY entries
    // from the local environment.
    let raw_env: Vec<String> = if !cli.env.is_empty() {
        cli.env.clone()
    } else {
        cfg.client.env.clone().unwrap_or_default()
    };
    let mut env_vars: Vec<String> = raw_env
        .into_iter()
        .filter_map(|e| {
            if e.contains('=') {
                Some(e)
            } else {
                std::env::var(&e).ok().map(|v| format!("{e}={v}"))
            }
        })
        .collect();

    // Automatically forward terminal/locale variables (mirrors SSH's SendEnv LANG LC_*).
    // COLORTERM and TERM_PROGRAM let TUI programs (btop, delta, fzf, …) pick the
    // right color depth; LANG/LC_* supply the locale.
    // Prepend so explicit --env entries take precedence.
    let locale_keys = [
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "LC_COLLATE",
        "LC_MESSAGES",
        "LC_MONETARY",
        "LC_NUMERIC",
        "LC_TIME",
        "COLORTERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
    ];
    let mut locale_prefix: Vec<String> = locale_keys
        .iter()
        .filter(|k| !env_vars.iter().any(|e| e.starts_with(&format!("{k}="))))
        .filter_map(|k| std::env::var(k).ok().map(|v| format!("{k}={v}")))
        .collect();
    locale_prefix.append(&mut env_vars);
    let env_vars = locale_prefix;

    let session_id = generate_session_id();
    let passkey = generate_passkey();
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());

    vlog!(
        cli.verbose,
        1,
        "[etr] Connecting to {} via SSH to bootstrap session...",
        target
    );

    let remote_command: String = cli.command.join(" ");

    let x11_enabled = cli.x11
        || cli.x11_trusted
        || cfg.client.x11.unwrap_or(false)
        || cfg.client.x11_trusted.unwrap_or(false);

    let (server_port, server_cert) = match bootstrap_ssh(
        &target,
        ssh_port,
        &session_id,
        &passkey,
        &term,
        &server_path,
        cli.server_log_path
            .as_deref()
            .or(cfg.client.server_log_path.as_deref()),
        &env_vars,
        if remote_command.is_empty() {
            None
        } else {
            Some(remote_command.as_str())
        },
        x11_enabled,
        cli.verbose,
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[etr] {e}");
            std::process::exit(1);
        }
    };

    vlog!(cli.verbose, 2, "[etr] etrs bound to port {server_port}");

    let session = Arc::new(Mutex::new(SessionState::new(session_id, passkey.clone())));

    let mut x11_auth_proto = String::new();
    let mut x11_auth_cookie = Vec::new();
    if x11_enabled {
        match std::env::var("DISPLAY") {
            Ok(disp) => match get_xauth_cookie(&disp) {
                Ok((proto, cookie)) => {
                    x11_auth_proto = proto;
                    x11_auth_cookie = cookie;
                }
                Err(e) => {
                    eprintln!("[etr] warning: X11 cookie extraction failed: {e}");
                }
            },
            Err(_) => {
                eprintln!(
                    "[etr] error: X11 forwarding requested but DISPLAY environment variable is not set"
                );
                std::process::exit(1);
            }
        }
    }

    if let Err(e) = run_connection_loop(
        target,
        server_port,
        server_cert,
        passkey,
        session_id,
        session,
        forward_specs,
        reverse_forward_specs,
        gateway_ports,
        !remote_command.is_empty(),
        x11_enabled,
        x11_auth_proto,
        x11_auth_cookie,
        cli.verbose,
    )
    .await
    {
        eprintln!("[etr] {e}");
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
    rand::random()
}

fn generate_passkey() -> String {
    use rand::Rng;
    rand::rng()
        .sample_iter(rand::distr::Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

/// SSH to the target, start `etrs`, send session credentials, and read back
/// the QUIC port and server cert DER from etrs stdout.
#[allow(clippy::too_many_arguments)]
fn bootstrap_ssh(
    target: &str,
    ssh_port: u16,
    session_id: &[u8; 16],
    passkey: &str,
    term: &str,
    server_path: &str,
    server_log_path: Option<&str>,
    env_vars: &[String],
    remote_command: Option<&str>,
    x11_enabled: bool,
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
    if let Some(log_path) = server_log_path {
        cmd.arg("--log-path").arg(log_path);
    }
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
    for kv in env_vars {
        stdin.write_all(format!("{kv}\n").as_bytes())?;
    }
    if x11_enabled {
        stdin.write_all(b"ETRX11:true\n")?;
    }
    if let Some(cmd) = remote_command {
        stdin.write_all(format!("ETRCMD:{cmd}\n").as_bytes())?;
    }
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
    reverse_forward_specs: Vec<String>,
    gateway_ports: bool,
    has_remote_command: bool,
    x11_enabled: bool,
    x11_auth_proto: String,
    x11_auth_cookie: Vec<u8>,
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
    let mut endpoint =
        quinn::Endpoint::client(bind_addr.parse().unwrap()).map_err(io::Error::other)?;
    endpoint.set_default_client_config(cli_cfg);

    // Single stdin reader shared across all reconnect iterations.
    let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(1000);
    let stdin_rx = Arc::new(Mutex::new(stdin_rx));

    // ~. triggers this to exit the reconnect loop.
    let (escape_tx, escape_rx) = tokio::sync::watch::channel(false);

    let _stdin_reader = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut buf = [0u8; 1024];
        // `~` is common in shell input, so only recognise it at line-start
        // (mirrors ssh ~. behaviour).
        let mut at_line_start = true;
        let mut escape_pending = false;
        while let Ok(n) = std::io::stdin().read(&mut buf) {
            if n == 0 {
                break;
            }
            if verbose >= 3
                && let Some(f) = LOG_FILE.get()
            {
                let hex: String = buf[..n]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                let _ = writeln!(f.lock().unwrap(), "[etr] stdin raw bytes: {hex}");
            }
            let mut out = Vec::with_capacity(n);
            for &b in &buf[..n] {
                if escape_pending {
                    escape_pending = false;
                    match b {
                        b'.' => {
                            // ~. — signal force-disconnect and stop reading.
                            let _ = escape_tx.send(true);
                            return;
                        }
                        b if b == ESCAPE_CHAR => {
                            // ~~ — send a literal ~.
                            out.push(ESCAPE_CHAR);
                            at_line_start = false;
                        }
                        _ => {
                            // Unknown sequence — forward both bytes verbatim.
                            out.push(ESCAPE_CHAR);
                            out.push(b);
                            at_line_start = matches!(b, b'\r' | b'\n');
                        }
                    }
                } else if b == ESCAPE_CHAR && at_line_start {
                    escape_pending = true;
                } else {
                    out.push(b);
                    at_line_start = matches!(b, b'\r' | b'\n');
                }
            }
            if !out.is_empty() && stdin_tx.blocking_send(out).is_err() {
                break;
            }
        }
    });

    let mut first = true;
    let mut escape_rx = escape_rx;
    // Track whether the terminal is currently in raw mode so reconnect messages
    // can use \r\n (raw) vs \n (cooked) and so we don't over-call disable_raw_mode.
    let mut in_raw = false;

    'reconnect: loop {
        if !first {
            // Stay in raw mode if we were already in it so ~. is
            // recognised immediately (no trailing Enter required).
            if in_raw {
                eprint!("[etr] Reconnecting to {server_addr}...  (Enter ~. to force-quit)\r\n");
            } else {
                eprintln!("[etr] Reconnecting to {server_addr}...  (Enter ~. to force-quit)");
            }
            vlog!(verbose, 2, "[etr] Reconnect delay 2s");
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                Ok(_) = escape_rx.wait_for(|&v| v) => {
                    if in_raw {
                        IN_RAW_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
                        let _ = disable_raw_mode();
                        eprint!("\r\n[etr] Disconnected (~.).\r\n");
                    } else {
                        eprintln!("[etr] Disconnected (~.).");
                    }
                    return Ok(());
                }
            }
        }
        first = false;

        vlog!(
            verbose,
            2,
            "[etr] Connecting  session={}",
            hex_encode(&session_id)
        );

        let connecting = match endpoint.connect(server_addr, "etr") {
            Ok(c) => c,
            Err(e) => {
                vlog!(verbose, 1, "[etr] Connect error: {e}");
                if has_remote_command {
                    eprintln!("[etr] Failed to connect: {e}");
                    std::process::exit(1);
                }
                continue 'reconnect;
            }
        };
        // Also poll escape here so ~. is responsive during the connect wait.
        let conn = tokio::select! {
            r = tokio::time::timeout(Duration::from_secs(15), connecting) => match r {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    vlog!(verbose, 1, "[etr] QUIC handshake failed: {e}");
                    if has_remote_command {
                        eprintln!("[etr] Failed to connect: {e}");
                        std::process::exit(1);
                    }
                    continue 'reconnect;
                }
                Err(_) => {
                    vlog!(verbose, 1, "[etr] QUIC connect timed out");
                    if has_remote_command {
                        eprintln!("[etr] Connection timed out.");
                        std::process::exit(1);
                    }
                    continue 'reconnect;
                }
            },
            Ok(_) = escape_rx.wait_for(|&v| v) => {
                if in_raw {
                    IN_RAW_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
                    let _ = disable_raw_mode();
                    eprint!("\r\n[etr] Disconnected (~.).\r\n");
                } else {
                    eprintln!("[etr] Disconnected (~.).");
                }
                return Ok(());
            }
        };

        vlog!(verbose, 2, "[etr] QUIC connected to {server_addr}");
        vlog!(verbose, 2, "[etr] {}", quic::tls_info());

        enable_raw_mode().unwrap();
        IN_RAW_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
        in_raw = true;
        let result = tokio::select! {
            r = run_session(
                conn,
                session_id,
                passkey.clone(),
                Arc::clone(&session),
                Arc::clone(&stdin_rx),
                forward_specs.clone(),
                reverse_forward_specs.clone(),
                gateway_ports,
                x11_enabled,
                x11_auth_proto.clone(),
                x11_auth_cookie.clone(),
                verbose,
            ) => r,
            Ok(_) = escape_rx.wait_for(|&v| v) => {
                IN_RAW_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
                let _ = disable_raw_mode();
                eprint!("\r\n[etr] Disconnected (~.).\r\n");
                std::process::exit(0);
            }
        };

        match result {
            Ok(_) => {
                IN_RAW_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
                let _ = disable_raw_mode();
                vlog!(verbose, 1, "[etr] Connection closed cleanly.");
                std::process::exit(0);
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {
                IN_RAW_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
                let _ = disable_raw_mode();
                vlog!(verbose, 1, "[etr] Connection closed cleanly.");
                std::process::exit(0);
            }
            Err(e) => {
                // For remote commands: exit rather than reconnect.  The command
                // has finished (or the server is gone), so there is nothing to
                // reconnect to.  Restore the terminal before printing.
                if has_remote_command {
                    IN_RAW_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
                    let _ = disable_raw_mode();
                    eprintln!("\n[etr] Session ended: {e}");
                    std::process::exit(1);
                }
                // Keep raw mode ON during reconnect so ~. fires immediately.
                eprint!("\r\n[etr] Connection lost.\r\n");
                if let Some(f) = LOG_FILE.get() {
                    let _ = writeln!(f.lock().unwrap(), "[etr] Connection lost.");
                }
                vlog!(verbose, 1, "[etr] Session dropped: {e:?}");
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    conn: quinn::Connection,
    session_id: [u8; 16],
    passkey: String,
    session: Arc<Mutex<SessionState>>,
    stdin_rx: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
    forward_specs: Vec<ForwardSpec>,
    reverse_forward_specs: Vec<String>,
    gateway_ports: bool,
    x11_enabled: bool,
    x11_auth_proto: String,
    x11_auth_cookie: Vec<u8>,
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
            reverse_forwards: reverse_forward_specs.clone(),
            gateway_ports,
            x11_enabled,
            x11_auth_proto: x11_auth_proto.clone(),
            x11_auth_cookie: x11_auth_cookie.clone(),
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

    vlog!(
        verbose,
        1,
        "\r\n[etr] Connected. Session active.  (Escape: ~. to disconnect)"
    );

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
                    vlog!(
                        verbose,
                        3,
                        "[etr] pty←server seq={seq} bytes={}",
                        data.len()
                    );
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
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "server connection dropped",
                    ));
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
            vlog!(
                verbose,
                3,
                "[etr] stdin→server seq={seq} bytes={}",
                payload.len()
            );
            if quic::write_pty_chunk(&mut pty_send, seq, &payload)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // ── Control reader task: ctrl_recv → dispatch ─────────────────────────
    let session_ctrl = Arc::clone(&session);
    let mut ctrl_reader_task: tokio::task::JoinHandle<io::Result<()>> = tokio::spawn(async move {
        loop {
            match quic::read_msg(&mut ctrl_recv).await {
                Ok(Some(env)) => match env.payload {
                    Some(Payload::Disconnect(_)) => {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "clean disconnect from server",
                        ));
                    }
                    Some(Payload::Heartbeat(hb)) => {
                        session_ctrl
                            .lock()
                            .await
                            .apply_server_acks(&hb.last_received_seq);
                        vlog!(
                            verbose,
                            3,
                            "[etr] hb←server acks={:?}",
                            hb.last_received_seq
                        );
                    }
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
    let session_hb = Arc::clone(&session);
    let mut ctrl_send_task: tokio::task::JoinHandle<io::Result<()>> = tokio::spawn(async move {
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
                    let last_received_seq = session_hb.lock().await.last_received_map();
                    vlog!(verbose, 3, "[etr] hb→server acks={last_received_seq:?}");
                    let env = Envelope {
                        payload: Some(Payload::Heartbeat(Heartbeat { last_received_seq })),
                    };
                    quic::write_msg(&mut ctrl_send, &env).await?;
                }
                Some(tr) = resize_rx.recv() => {
                    vlog!(verbose, 3, "[etr] resize {}x{}", tr.cols, tr.rows);
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
                tokio::spawn(run_tcp_acceptor_quic(spec2, conn2, gateway_ports, verbose))
            }
            ForwardProto::Udp => tokio::spawn(run_udp_forward_client_quic(
                spec2,
                conn2,
                gateway_ports,
                verbose,
            )),
        };
        fwd_handles.push(handle);
    }

    // ── Reverse forward stream acceptor task ──────────────────────────────
    let conn_clone = conn.clone();
    let verbose_clone = verbose;
    let reverse_acceptor_handle = tokio::spawn(async move {
        while let Ok((mut quic_send, mut quic_recv)) = conn_clone.accept_bi().await {
            let verbose = verbose_clone;
            tokio::spawn(async move {
                let tag = match quic::read_tag(&mut quic_recv).await {
                    Ok(t) => t,
                    Err(_) => return,
                };
                if tag != TAG_FORWARD {
                    vlog!(
                        verbose,
                        1,
                        "[etr] reverse forward error: expected TAG_FORWARD, got 0x{tag:02x}"
                    );
                    return;
                }
                let so = match quic::read_msg(&mut quic_recv).await {
                    Ok(Some(env)) => match env.payload {
                        Some(Payload::StreamOpen(so)) => so,
                        _ => return,
                    },
                    _ => return,
                };
                if so.stream_type == etr::protocol::StreamType::X11 as i32 {
                    let local_display = match std::env::var("DISPLAY") {
                        Ok(d) => d,
                        Err(_) => {
                            vlog!(
                                verbose,
                                1,
                                "[etr] X11: local DISPLAY env var not set, rejecting stream"
                            );
                            let _ = quic_send.finish();
                            return;
                        }
                    };
                    match connect_local_x11(&local_display, verbose).await {
                        Ok(stream) => {
                            run_x11_connection_quic(stream, quic_send, quic_recv).await;
                        }
                        Err(e) => {
                            vlog!(
                                verbose,
                                1,
                                "[etr] X11: failed to connect to local display {local_display}: {e}"
                            );
                            let _ = quic_send.finish();
                        }
                    }
                    return;
                }
                let proto = ForwardProto::try_from(so.forward_proto).unwrap_or(ForwardProto::Tcp);
                match proto {
                    ForwardProto::Tcp => {
                        let addr = format!("{}:{}", so.remote_host, so.remote_port);
                        use tokio::net::TcpStream;
                        vlog!(verbose, 2, "[etr] connecting to local TCP target {addr}");
                        let tcp = match TcpStream::connect(&addr).await {
                            Ok(t) => t,
                            Err(e) => {
                                vlog!(
                                    verbose,
                                    1,
                                    "[etr] failed to connect to local target {addr}: {e}"
                                );
                                let _ = quic_send.finish();
                                return;
                            }
                        };
                        run_tcp_connection_quic(tcp, quic_send, quic_recv).await;
                    }
                    ForwardProto::Udp => {
                        let addr_str = format!("{}:{}", so.remote_host, so.remote_port);
                        let addr: std::net::SocketAddr =
                            match etr::forward::resolve_udp_target(&addr_str).await {
                                Some(a) => a,
                                None => {
                                    vlog!(
                                        verbose,
                                        1,
                                        "[etr] UDP reverse fwd: cannot resolve {addr_str}"
                                    );
                                    let _ = quic_send.finish();
                                    return;
                                }
                            };
                        vlog!(verbose, 2, "[etr] forwarding UDP reverse stream to {addr}");
                        use std::collections::HashMap;
                        use std::time::Instant;
                        use tokio::net::UdpSocket;
                        use tokio::sync::mpsc as udp_mpsc;
                        let bind_addr = if addr.is_ipv6() {
                            "[::]:0"
                        } else {
                            "0.0.0.0:0"
                        };

                        // Each external sender (peer_addr:peer_port from QUIC envelope) gets
                        // its own ephemeral socket toward the local target so replies can be
                        // routed back to the correct sender.
                        const SENDER_IDLE: Duration = Duration::from_secs(30);

                        let (reply_tx, mut reply_rx) = udp_mpsc::channel::<Envelope>(256);

                        let mut quic_send = quic_send;
                        let verbose_reply = verbose;
                        let mut reply_task = tokio::spawn(async move {
                            while let Some(env) = reply_rx.recv().await {
                                if quic::write_msg(&mut quic_send, &env).await.is_err() {
                                    vlog!(
                                        verbose_reply,
                                        1,
                                        "[etr] UDP reverse fwd: failed to write reply to QUIC"
                                    );
                                    break;
                                }
                            }
                        });

                        let verbose_send = verbose;
                        let mut send_task = tokio::spawn(async move {
                            let mut sender_map: HashMap<(String, u32), (Arc<UdpSocket>, Instant)> =
                                HashMap::new();

                            while let Ok(Some(env)) = quic::read_msg(&mut quic_recv).await {
                                if let Some(Payload::UdpDatagram(dg)) = env.payload {
                                    if dg.peer_addr.is_empty() {
                                        continue;
                                    }
                                    let key = (dg.peer_addr.clone(), dg.peer_port);
                                    let now = Instant::now();

                                    sender_map.retain(|_, (_, last)| {
                                        now.duration_since(*last) < SENDER_IDLE
                                    });

                                    let socket = if let Some((sock, last)) =
                                        sender_map.get_mut(&key)
                                    {
                                        *last = now;
                                        Arc::clone(sock)
                                    } else {
                                        let sock = match UdpSocket::bind(bind_addr).await {
                                            Ok(s) => Arc::new(s),
                                            Err(e) => {
                                                vlog!(
                                                    verbose_send,
                                                    1,
                                                    "[etr] UDP reverse fwd: bind failed for sender {}: {e}",
                                                    key.0
                                                );
                                                continue;
                                            }
                                        };
                                        sender_map.insert(key.clone(), (Arc::clone(&sock), now));

                                        // Per-sender reply task: reads local-target replies
                                        // and forwards them back with the original sender's
                                        // peer_addr/peer_port so the server routes correctly.
                                        let peer_addr = key.0.clone();
                                        let peer_port = key.1;
                                        let sock_r = Arc::clone(&sock);
                                        let tx = reply_tx.clone();
                                        let vr = verbose_send;
                                        tokio::spawn(async move {
                                            let mut buf = vec![0u8; 65535];
                                            while let Ok(Ok((n, src))) = tokio::time::timeout(
                                                SENDER_IDLE,
                                                sock_r.recv_from(&mut buf),
                                            )
                                            .await
                                            {
                                                vlog!(
                                                    vr,
                                                    3,
                                                    "[etr] UDP reverse fwd: {n} bytes from local target {src} → sender {peer_addr}:{peer_port}"
                                                );
                                                let env = Envelope {
                                                    payload: Some(Payload::UdpDatagram(
                                                        UdpDatagram {
                                                            peer_addr: peer_addr.clone(),
                                                            peer_port,
                                                            data: buf[..n].to_vec(),
                                                        },
                                                    )),
                                                };
                                                if tx.send(env).await.is_err() {
                                                    break;
                                                }
                                            }
                                        });

                                        sock
                                    };

                                    vlog!(
                                        verbose_send,
                                        3,
                                        "[etr] UDP reverse fwd: forwarding {} bytes to local target {addr}",
                                        dg.data.len()
                                    );
                                    let _ = socket.send_to(&dg.data, &addr).await;
                                }
                            }
                        });

                        tokio::select! {
                            _ = &mut reply_task => {}
                            _ = &mut send_task => {}
                        }
                        reply_task.abort();
                        send_task.abort();
                        vlog!(verbose, 2, "[etr] UDP reverse fwd stream ended");
                    }
                }
            });
        }
    });
    fwd_handles.push(reverse_acceptor_handle);

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

/// Accept local TCP connections on the addresses resolved by `spec.get_bind_addresses(gateway)`
/// and open one QUIC forward stream per connection toward the spec's remote host/port.
///
/// `gateway` mirrors the `-g` / `--gateway-ports` flag: when `true`, the listener binds
/// a dual-stack `[::]` socket (all interfaces); when `false` it binds `127.0.0.1` + `[::1]`
/// or whatever explicit bind address the spec contains.
async fn run_tcp_acceptor_quic(
    spec: ForwardSpec,
    conn: quinn::Connection,
    gateway: bool,
    verbose: u8,
) {
    use tokio::net::TcpListener;

    let bind_addrs = spec.get_bind_addresses(gateway);
    let mut listeners = Vec::new();
    for addr in &bind_addrs {
        let target = format!("{addr}:{}", spec.local_port);
        match TcpListener::bind(&target).await {
            Ok(l) => listeners.push(l),
            Err(e) => {
                vlog!(verbose, 1, "[etr] TCP bind to {target} failed: {e}");
            }
        }
    }

    if listeners.is_empty() {
        eprintln!(
            "[etr] cannot bind TCP port {} on any of {:?}",
            spec.local_port, bind_addrs
        );
        return;
    }

    vlog!(
        verbose,
        1,
        "[etr] TCP forward  local:{} → {}:{}",
        spec.local_port,
        spec.remote_host,
        spec.remote_port
    );

    let run_loop = |listener: TcpListener,
                    conn: quinn::Connection,
                    spec: ForwardSpec,
                    verbose: u8| async move {
        loop {
            let (tcp_stream, peer) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    vlog!(verbose, 1, "[etr] TCP accept error: {e}");
                    break;
                }
            };
            let _ = tcp_stream.set_nodelay(true);
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
    };

    let mut join_handles = Vec::new();
    for listener in listeners {
        join_handles.push(tokio::spawn(run_loop(
            listener,
            conn.clone(),
            spec.clone(),
            verbose,
        )));
    }

    for h in join_handles {
        let _ = h.await;
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

/// Open one QUIC forward stream for a UDP `-L` spec and pipe local datagrams through it.
///
/// Binds one local UDP socket per address returned by `spec.get_bind_addresses(gateway)`.
/// `gateway` mirrors the `-g` / `--gateway-ports` flag: when `true`, the socket binds
/// a dual-stack `[::]` socket (all interfaces); when `false` it binds `127.0.0.1` + `[::1]`
/// or whatever explicit bind address the spec contains.
async fn run_udp_forward_client_quic(
    spec: ForwardSpec,
    conn: quinn::Connection,
    gateway: bool,
    verbose: u8,
) {
    use tokio::net::UdpSocket;

    let bind_addrs = spec.get_bind_addresses(gateway);
    let mut sockets = Vec::new();
    for addr in &bind_addrs {
        let target = format!("{addr}:{}", spec.local_port);
        match UdpSocket::bind(&target).await {
            Ok(s) => sockets.push(s),
            Err(e) => {
                vlog!(verbose, 1, "[etr] UDP bind to {target} failed: {e}");
            }
        }
    }

    if sockets.is_empty() {
        eprintln!(
            "[etr] cannot bind UDP port {} on any of {:?}",
            spec.local_port, bind_addrs
        );
        return;
    }

    vlog!(
        verbose,
        1,
        "[etr] UDP forward  local:{} → {}:{}",
        spec.local_port,
        spec.remote_host,
        spec.remote_port
    );

    let mut join_handles = Vec::new();
    for socket in sockets {
        join_handles.push(tokio::spawn(run_udp_forward_client_socket(
            socket,
            spec.clone(),
            conn.clone(),
            verbose,
        )));
    }

    for h in join_handles {
        let _ = h.await;
    }
}

async fn run_udp_forward_client_socket(
    local_socket: tokio::net::UdpSocket,
    spec: ForwardSpec,
    conn: quinn::Connection,
    verbose: u8,
) {
    let local_socket = Arc::new(local_socket);
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

enum X11Stream {
    Unix(tokio::net::UnixStream),
    Tcp(tokio::net::TcpStream),
}

async fn connect_local_x11(display_str: &str, verbose: u8) -> io::Result<X11Stream> {
    let display = X11Display::parse(display_str)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    match display {
        X11Display::Unix(n) => {
            let path = format!("/tmp/.X11-unix/X{n}");
            vlog!(
                verbose,
                2,
                "[etr] X11: connecting to local Unix socket {path}"
            );
            let s = tokio::net::UnixStream::connect(&path).await?;
            Ok(X11Stream::Unix(s))
        }
        X11Display::Path(p) => {
            vlog!(verbose, 2, "[etr] X11: connecting to local Unix path {p}");
            let s = tokio::net::UnixStream::connect(&p).await?;
            Ok(X11Stream::Unix(s))
        }
        X11Display::Tcp(host, port) => {
            let addr = format!("{host}:{port}");
            vlog!(
                verbose,
                2,
                "[etr] X11: connecting to local TCP address {addr}"
            );
            let s = tokio::net::TcpStream::connect(&addr).await?;
            Ok(X11Stream::Tcp(s))
        }
    }
}

async fn run_x11_connection_quic(
    stream: X11Stream,
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    match stream {
        X11Stream::Unix(s) => {
            let (mut r, mut w) = s.into_split();
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
        X11Stream::Tcp(s) => {
            let (mut r, mut w) = s.into_split();
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
    }
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

    #[test]
    fn test_log_path_override() {
        let cli = Cli::try_parse_from(["etr", "--log-path", "/tmp/client.log", "host"]).unwrap();
        assert_eq!(
            cli.log_path,
            Some(std::path::PathBuf::from("/tmp/client.log"))
        );
    }

    #[test]
    fn test_server_log_path_override() {
        let cli =
            Cli::try_parse_from(["etr", "--server-log-path", "/tmp/server.log", "host"]).unwrap();
        assert_eq!(cli.server_log_path.as_deref(), Some("/tmp/server.log"));
    }

    #[test]
    fn test_remote_command_single_arg() {
        let cli = Cli::try_parse_from(["etr", "host", "distrobox -- btop"]).unwrap();
        assert_eq!(cli.target.as_deref(), Some("host"));
        assert_eq!(cli.command, vec!["distrobox -- btop"]);
    }

    #[test]
    fn test_remote_command_multi_word() {
        let cli = Cli::try_parse_from(["etr", "host", "ls", "-la", "/tmp"]).unwrap();
        assert_eq!(cli.target.as_deref(), Some("host"));
        assert_eq!(cli.command, vec!["ls", "-la", "/tmp"]);
        assert_eq!(cli.command.join(" "), "ls -la /tmp");
    }

    #[test]
    fn test_remote_command_empty_without_args() {
        let cli = Cli::try_parse_from(["etr", "host"]).unwrap();
        assert!(cli.command.is_empty());
    }

    #[test]
    fn test_escape_char_value() {
        // SSH-style tilde escape; verify the constant is `~`.
        assert_eq!(ESCAPE_CHAR, b'~');
    }

    #[test]
    fn test_log_paths_fallback_to_config() {
        let toml = "[client]\nlog_path = \"/config/client.log\"\nserver_log_path = \"/config/server.log\"\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        let cli = Cli::try_parse_from(["etr", "host"]).unwrap();

        let log_path = cli
            .log_path
            .clone()
            .or_else(|| cfg.client.log_path.as_ref().map(std::path::PathBuf::from))
            .unwrap_or_else(client_log_path);

        let server_log_path = cli
            .server_log_path
            .as_deref()
            .or(cfg.client.server_log_path.as_deref());

        assert_eq!(log_path, std::path::PathBuf::from("/config/client.log"));
        assert_eq!(server_log_path, Some("/config/server.log"));
    }
}
