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

use etr::addrfam::AddrPref;
use etr::config::Config;
use etr::forward::ForwardSpec;
#[cfg(unix)]
use etr::forward::{X11Display, get_xauth_cookie};
use etr::protocol::{
    Envelope, ForwardProto, Heartbeat, Payload, SessionOpen, StreamOpen, TerminalResize,
    UdpDatagram,
};
use etr::quic::{self, TAG_CONTROL, TAG_FORWARD, TAG_PTY};
use etr::session::SessionState;

static LOG_FILE: std::sync::OnceLock<std::sync::Mutex<std::fs::File>> = std::sync::OnceLock::new();
static IN_RAW_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set once `enable_raw_mode` has actually succeeded, and never cleared.
///
/// Distinct from [`IN_RAW_MODE`], which tracks whether raw mode is in effect
/// *right now* (and which `restore_terminal` clears): this records whether the
/// terminal was ever ours to modify at all. When there is no controlling
/// terminal — cron, a systemd unit, CI, an agent shell — it stays `false` and
/// [`restore_terminal`] becomes a no-op, so the VT reset sequences are never
/// written into a stdout that is really a file or a pipe.
static RAW_EVER_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// The console input/output modes and input codepage in effect before etr
// touched anything, captured once by `capture_console_originals` so they can be
// restored verbatim on exit.  Restoring the exact modes is essential: crossterm's
// `disable_raw_mode` only ORs the line/echo/processed-input bits back, so the
// `ENABLE_VIRTUAL_TERMINAL_INPUT` we add in `enable_vt_console` would otherwise
// be left set — which breaks the *local* shell's Enter handling after etr exits.
#[cfg(windows)]
static CONSOLE_CAPTURED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(windows)]
static ORIG_INPUT_MODE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
#[cfg(windows)]
static OUTPUT_CAPTURED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(windows)]
static ORIG_OUTPUT_MODE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
#[cfg(windows)]
static ORIG_INPUT_CP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Cursor-safe VT resets emitted on *every* session exit.
///
/// A remote full-screen program (zellij, vim, less, …) switches the *local*
/// terminal into mouse-reporting, bracketed-paste, application-cursor-key,
/// application-keypad and hidden-cursor modes via escape sequences we relay to
/// stdout.  If the session dies before that program emits its own cleanup — a
/// hard drop (remote reboot) or a forced `~.` — those modes stay set and the
/// terminal is left unusable (mouse wheel spews escape sequences, cursor
/// hidden).  These are terminal-*emulator* modes, not console line/echo flags,
/// so `disable_raw_mode` does not undo them; we emit the resets ourselves.
///
/// Every reset here is idempotent *and* leaves the cursor where it is, so it is
/// safe to send even on a clean exit where nothing was left set.
const TERM_RESET_MODES: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[?1l\x1b>\x1b[?7h\x1b[?25h\x1b[0m";

/// Screen-restoring VT resets: leave the alternate screen buffer and reset the
/// scrolling region.  Both move the cursor to home per the VT spec, so these are
/// emitted *only* when the session ended uncleanly (hard drop / forced `~.`) and
/// a full-screen app may still hold the alternate screen — never on a clean exit
/// where the remote app already switched back (re-emitting them would reposition
/// the cursor).  Deliberately avoids a full RIS (`\x1bc`) so scrollback is kept.
const TERM_RESET_SCREEN: &[u8] = b"\x1b[?1049l\x1b[r";

/// Escape character for the client: `~` (0x7E), SSH-style.  Type it at the
/// start of a line followed by `.` to force-disconnect.  The line-start guard
/// prevents false triggers from `~` in shell paths or git refs.
const ESCAPE_CHAR: u8 = b'~';

/// Put the console into virtual-terminal mode.
///
/// On Windows the console, by default, hands raw byte reads the legacy key
/// codes (Backspace → `0x08`, no ESC sequences for arrows/function keys) and
/// does not interpret ANSI output.  A Unix PTY on the far end expects the
/// xterm conventions instead — notably Backspace → `0x7f` (DEL) — so without
/// this the remote `stty erase` (DEL) never matches what we send and Backspace
/// misbehaves.  Enabling `ENABLE_VIRTUAL_TERMINAL_INPUT` makes the console
/// translate keys into the same VT byte sequences a real terminal emits, and
/// `ENABLE_VIRTUAL_TERMINAL_PROCESSING` makes it render the remote's ANSI
/// output.  Call this after raw mode is enabled, on every (re)connect.
///
/// No-op on Unix, where the terminal already speaks VT natively.
#[cfg(windows)]
fn enable_vt_console() {
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCP,
        SetConsoleMode,
    };
    // The UTF-8 codepage.  We read the console input handle directly with
    // `ReadFile` (see `read_stdin`), which returns typed characters encoded in
    // the console *input* codepage.  Set it to UTF-8 so multi-byte input reaches
    // the remote as the UTF-8 the Unix PTY expects, rather than the legacy
    // OEM/ANSI codepage.
    const CP_UTF8: u32 = 65001;
    // SAFETY: standard Win32 console calls.  `GetStdHandle` returns a process
    // std handle; `GetConsoleMode` fails (returns 0) for non-console handles
    // (e.g. redirected stdio), so we only call `SetConsoleMode` on a handle it
    // confirmed is a console.  All pointers point to locals that outlive the call.
    unsafe {
        let h_in = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode: CONSOLE_MODE = 0;
        if GetConsoleMode(h_in, &mut mode) != 0 {
            SetConsoleMode(h_in, mode | ENABLE_VIRTUAL_TERMINAL_INPUT);
            SetConsoleCP(CP_UTF8);
        }
        let h_out = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut out_mode: CONSOLE_MODE = 0;
        if GetConsoleMode(h_out, &mut out_mode) != 0 {
            SetConsoleMode(h_out, out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

/// No-op on Unix: the terminal already delivers VT key sequences and renders ANSI.
#[cfg(not(windows))]
fn enable_vt_console() {}

/// Capture the console's original input/output modes and input codepage exactly
/// once, before etr changes any of them, so they can be restored verbatim on
/// exit.  Must be called before the first `enable_raw_mode`/`enable_vt_console`.
/// No-op on Unix (crossterm's `disable_raw_mode` fully restores termios there).
#[cfg(windows)]
fn capture_console_originals() {
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, GetConsoleCP, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };
    if CONSOLE_CAPTURED.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: standard Win32 console queries.  `GetConsoleMode` fails (returns 0)
    // for non-console handles, in which case we capture nothing and restore is a
    // no-op.  All pointers reference locals that outlive the call.
    unsafe {
        let h_in = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode: CONSOLE_MODE = 0;
        if GetConsoleMode(h_in, &mut mode) == 0 {
            return; // no real console (e.g. redirected stdin); nothing to restore
        }
        ORIG_INPUT_MODE.store(mode, Ordering::Relaxed);
        let cp = GetConsoleCP();
        if cp != 0 {
            ORIG_INPUT_CP.store(cp, Ordering::Relaxed);
        }
        let h_out = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut out_mode: CONSOLE_MODE = 0;
        if GetConsoleMode(h_out, &mut out_mode) != 0 {
            ORIG_OUTPUT_MODE.store(out_mode, Ordering::Relaxed);
            OUTPUT_CAPTURED.store(true, Ordering::Relaxed);
        }
        CONSOLE_CAPTURED.store(true, Ordering::Relaxed);
    }
}

#[cfg(not(windows))]
fn capture_console_originals() {}

/// Restore the console modes and input codepage captured by
/// `capture_console_originals`.  This is what actually clears the
/// `ENABLE_VIRTUAL_TERMINAL_INPUT` flag `enable_vt_console` set — crossterm's
/// `disable_raw_mode` does not — so the *local* shell's line input (Enter) works
/// again after etr exits.  No-op on Unix and if nothing was captured.
#[cfg(windows)]
fn restore_console_state() {
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCP, SetConsoleMode,
    };
    if !CONSOLE_CAPTURED.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: restoring modes/codepage captured earlier from the same handles.
    unsafe {
        let h_in = GetStdHandle(STD_INPUT_HANDLE);
        SetConsoleMode(h_in, ORIG_INPUT_MODE.load(Ordering::Relaxed));
        let cp = ORIG_INPUT_CP.load(Ordering::Relaxed);
        if cp != 0 {
            SetConsoleCP(cp);
        }
        if OUTPUT_CAPTURED.load(Ordering::Relaxed) {
            let h_out = GetStdHandle(STD_OUTPUT_HANDLE);
            SetConsoleMode(h_out, ORIG_OUTPUT_MODE.load(Ordering::Relaxed));
        }
    }
}

#[cfg(not(windows))]
fn restore_console_state() {}

/// Read a chunk of stdin bytes into `buf`, returning the number read (0 = EOF).
///
/// On Unix this is a plain `stdin().read`.  On Windows it reads the console
/// input handle directly with `ReadFile`, bypassing Rust std's `ReadConsoleW`
/// shim: with `ENABLE_VIRTUAL_TERMINAL_INPUT` on (set by `enable_vt_console`)
/// the console hands `ReadFile` the same per-keystroke VT byte stream a Unix
/// terminal emits.  The std shim instead batches input and drops bytes that
/// aren't valid UTF-8, which manifested as "special characters eaten" and the
/// "first line not echoed until Enter" bug (#54).
#[cfg(not(windows))]
fn read_stdin(buf: &mut [u8]) -> io::Result<usize> {
    use std::io::Read;
    std::io::stdin().read(buf)
}

#[cfg(windows)]
fn read_stdin(buf: &mut [u8]) -> io::Result<usize> {
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
    // SAFETY: `GetStdHandle` returns the process stdin handle, valid for the
    // process lifetime.  `ReadFile` writes at most `buf.len()` bytes into `buf`
    // and reports the count via `read`; both pointers reference locals/`buf`
    // that outlive the call.  A null overlapped pointer requests a synchronous
    // read, which is correct for the (synchronous) console handle.
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        let mut read: u32 = 0;
        let ok = ReadFile(
            h,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut read,
            std::ptr::null_mut(),
        );
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(read as usize)
    }
}

/// Return the local terminal to a sane state after a session ends for good
/// (clean exit, `~.`, or a hard drop we give up on).  Emits the VT resets while
/// output VT processing is still enabled, drops raw mode, then restores the
/// console's exact original modes + codepage (which is what clears the
/// `ENABLE_VIRTUAL_TERMINAL_INPUT` flag so the local shell's Enter works again).
///
/// `reset_screen` should be `true` only for unclean endings (forced `~.`, a hard
/// drop we abandon, a remote command whose TUI may still be up): it additionally
/// leaves the alternate screen and resets the scroll region ([`TERM_RESET_SCREEN`]),
/// which move the cursor.  On a clean exit pass `false` so the cursor is left
/// untouched, since the remote already restored the screen.
///
/// Only acts when raw/VT mode was actually entered — otherwise the escape bytes
/// would print literally on a console without VT processing enabled, or land in
/// a redirected stdout as 70 bytes of garbage ahead of the real output. That
/// precondition used to be the caller's to honour; it is now enforced here via
/// [`RAW_EVER_ENABLED`], because the no-controlling-terminal path reaches several
/// of these call sites and getting it wrong is invisible until someone pipes
/// `etr host 'cmd'` into a file.
fn restore_terminal(reset_screen: bool) {
    if !RAW_EVER_ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    IN_RAW_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
    {
        let mut out = io::stdout();
        // Leave the alternate screen first (back to the normal buffer), then
        // reset the cursor-safe modes on the buffer the user will actually see.
        if reset_screen {
            let _ = out.write_all(TERM_RESET_SCREEN);
        }
        let _ = out.write_all(TERM_RESET_MODES);
        let _ = out.flush();
    }
    // Drop crossterm's raw mode first (keeps its internal state consistent), then
    // restore the exact original console modes — the latter wins, and crucially
    // clears ENABLE_VIRTUAL_TERMINAL_INPUT, which crossterm's disable_raw_mode
    // leaves set.
    let _ = disable_raw_mode();
    restore_console_state();
}

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

    /// Prefer IPv4 when the host resolves to both families.
    /// A preference, not a restriction: IPv6 is still used if the host has no
    /// IPv4 address or none is routable.
    #[arg(short = '4', long, conflicts_with = "prefer_ipv6")]
    prefer_ipv4: bool,

    /// Prefer IPv6 when the host resolves to both families.
    /// A preference, not a restriction: IPv4 is still used if the host has no
    /// IPv6 address or none is routable.
    #[arg(short = '6', long, conflicts_with = "prefer_ipv4")]
    prefer_ipv6: bool,

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

    // CLI flags win over [client] address_family; both default to Auto, which
    // leaves every resolution path behaving exactly as it did before -4/-6 existed.
    let addr_pref = AddrPref::from_flags(cli.prefer_ipv4, cli.prefer_ipv6)
        .or(AddrPref::from_config(cfg.client.address_family.as_deref()));

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

    // `ssh -4`/`-6` are restrictions, not preferences: passing one for a family
    // the target does not have turns `etr -6 host` into a bootstrap failure
    // instead of a fallback.  So resolve first and only pass the flag when the
    // host really has an address of that family.  A host that does not resolve
    // locally (an ssh_config `Host` alias, say) gets no flag, leaving ssh's own
    // resolution untouched.
    let host_for_lookup = match target.find('@') {
        Some(idx) => &target[idx + 1..],
        None => target.as_str(),
    };
    let ssh_family_flag = if addr_pref == AddrPref::Auto {
        None
    } else if etr::addrfam::family_available(&format!("{host_for_lookup}:{ssh_port}"), addr_pref)
        .await
    {
        addr_pref.ssh_flag()
    } else {
        vlog!(
            cli.verbose,
            1,
            "[etr] {} requested, but {host_for_lookup} has no such address — \
             letting ssh choose the family",
            addr_pref.as_str()
        );
        None
    };

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
        ssh_family_flag,
        addr_pref,
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

    #[cfg(windows)]
    if x11_enabled {
        eprintln!("[etr] error: X11 forwarding (-X/-Y) is not supported on Windows");
        std::process::exit(1);
    }

    #[cfg_attr(windows, allow(unused_mut))]
    let mut x11_auth_proto = String::new();
    #[cfg_attr(windows, allow(unused_mut))]
    let mut x11_auth_cookie = Vec::new();
    #[cfg(unix)]
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
        addr_pref,
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
    ssh_family_flag: Option<&str>,
    addr_pref: AddrPref,
    verbose: u8,
) -> io::Result<(u16, Vec<u8>)> {
    let session_id_hex = hex_encode(session_id);
    let v_flag = match verbose {
        0 => String::new(),
        n => format!("-{}", "v".repeat(n as usize)),
    };
    let mut cmd = Command::new("ssh");
    cmd.arg("-p").arg(ssh_port.to_string());
    // Only set when the caller has already confirmed the target has an address
    // of that family — see the call site.
    if let Some(flag) = ssh_family_flag {
        cmd.arg(flag);
    }
    cmd.arg(target);
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
    // Tell the server which family to prefer when it resolves `-L` targets, which
    // it does on its own side.  Old servers skip the line (it has no `=`), so
    // this stays compatible in both directions.
    if let Some(v) = addr_pref.wire() {
        stdin.write_all(format!("ETRPREFER:{v}\n").as_bytes())?;
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
    addr_pref: AddrPref,
    verbose: u8,
) -> io::Result<()> {
    // Snapshot the console's original modes/codepage before anything (raw mode,
    // VT-input) changes them, so `restore_terminal` can put it back exactly.
    capture_console_originals();

    let host = if let Some(idx) = target.find('@') {
        &target[idx + 1..]
    } else {
        &target
    };
    // Pick the QUIC peer address: preferred family first, and among the
    // candidates the first the kernel actually has a route to.  The routing
    // probe matters even without a flag — a host whose AAAA record comes back
    // first on a machine with no IPv6 route used to fail the connect outright
    // instead of falling through to the A record.
    let server_addr = etr::addrfam::resolve_preferred(&format!("{host}:{port}"), addr_pref)
        .await
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("could not resolve host: {host}"),
            )
        })?;
    vlog!(
        verbose,
        2,
        "[etr] server address {server_addr} (family preference: {})",
        addr_pref.as_str()
    );

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

    // Windows only: the reader must not issue its first `ReadFile` until raw +
    // VT-input mode is enabled.  A `ReadFile` issued while the console is still
    // in cooked/line mode stays line-buffered for that whole read, so the first
    // line would be held until Enter (issue #54 — "first line not echoed until
    // Enter").  Raw mode is enabled per-connect (after the QUIC handshake), so
    // we gate the reader on a one-shot signal fired right after the first
    // `enable_raw_mode` + `enable_vt_console`.  Unix has no such coupling (and
    // never exhibited the bug), so its reader starts immediately as before.
    #[cfg(windows)]
    let (raw_ready_tx, raw_ready_rx) = std::sync::mpsc::channel::<()>();

    let _stdin_reader = tokio::task::spawn_blocking(move || {
        // Wait until the console is in raw + VT-input mode before the first read.
        #[cfg(windows)]
        let _ = raw_ready_rx.recv();
        let mut buf = [0u8; 1024];
        // `~` is common in shell input, so only recognise it at line-start
        // (mirrors ssh ~. behaviour).
        let mut at_line_start = true;
        let mut escape_pending = false;
        while let Ok(n) = read_stdin(&mut buf) {
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
    // Windows only: fired once, right after raw + VT mode is first enabled, to
    // release the gated stdin reader (see the reader spawn above).
    #[cfg(windows)]
    let mut raw_ready_tx = Some(raw_ready_tx);

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
                    // Only restore (emit the VT reset) if we ever entered raw/VT
                    // mode — otherwise the escape bytes would print literally.
                    // Unclean ending: reset the screen too.
                    if in_raw {
                        restore_terminal(true);
                    }
                    eprintln!("[etr] Disconnected (~.).");
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
                    restore_terminal(true);
                }
                eprintln!("[etr] Disconnected (~.).");
                return Ok(());
            }
        };

        vlog!(verbose, 2, "[etr] QUIC connected to {server_addr}");
        vlog!(verbose, 2, "[etr] {}", quic::tls_info());

        // Raw mode needs a controlling terminal: crossterm opens `/dev/tty`, which
        // fails with ENXIO under cron, a systemd unit, a CI job or an agent shell.
        // This used to be `.unwrap()`, so those contexts got a Rust panic and
        // exit 101 instead of anything actionable.
        match enable_raw_mode() {
            Ok(()) => {
                // On Windows, raw mode alone still leaves the console emitting legacy key
                // codes (Backspace → 0x08) and not rendering ANSI; switch it to VT mode so
                // we speak the same conventions as the remote Unix PTY.  No-op on Unix.
                enable_vt_console();
                RAW_EVER_ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
                IN_RAW_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
                in_raw = true;
            }
            // A remote command does not need a local terminal: there is nothing to
            // put in raw mode and nothing to interact with, only output to relay.
            // Carrying on makes `etr host 'cmd'` work from a script, a cron job or
            // CI. Everything downstream already copes — `terminal::size()` is
            // handled with `if let Ok`, so no resize is sent and the server keeps
            // its default PTY size, and `restore_terminal` is a no-op because
            // `RAW_EVER_ENABLED` stays false (emitting the VT resets here would
            // inject escape bytes into a redirected stdout, corrupting the very
            // command output this path exists to deliver).
            Err(e) if has_remote_command => {
                vlog!(
                    verbose,
                    1,
                    "[etr] No controlling terminal ({e}); running without raw mode"
                );
            }
            // An interactive session, though, has nothing to interact with. Failing
            // loudly beats connecting to a shell nobody can type at — and which
            // would never exit on its own, so it would hang rather than return.
            Err(e) => {
                eprintln!("[etr] Cannot enter raw mode: no controlling terminal ({e}).");
                // Name the target the user actually typed, not `server_addr` —
                // that is the resolved QUIC address (`[::1]:53167`), which is
                // useless as advice and looks like a different host entirely.
                eprintln!(
                    "[etr] An interactive session needs one. To run a command without a\n\
                     [etr] terminal, pass it as arguments instead:  etr {target} <command>"
                );
                std::process::exit(1);
            }
        }
        // Release the stdin reader now that raw + VT mode is active, so its first
        // read is per-keystroke rather than a line-buffered cooked read (#54).
        // Fired on the degraded path too: the reader is gated on this signal, so
        // skipping it would leave a Windows run with no terminal blocked forever on
        // a reader that never starts — and piped stdin still has to be relayed.
        #[cfg(windows)]
        if let Some(tx) = raw_ready_tx.take() {
            let _ = tx.send(());
        }
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
                has_remote_command,
                addr_pref,
                verbose,
            ) => r,
            Ok(_) = escape_rx.wait_for(|&v| v) => {
                restore_terminal(true);
                eprintln!("[etr] Disconnected (~.).");
                std::process::exit(0);
            }
        };

        match result {
            Ok(_) => {
                // Clean exit: the remote shell already restored the screen, so
                // don't touch the cursor — just reset emulator modes.
                restore_terminal(false);
                vlog!(verbose, 1, "[etr] Connection closed cleanly.");
                std::process::exit(0);
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {
                restore_terminal(false);
                vlog!(verbose, 1, "[etr] Connection closed cleanly.");
                std::process::exit(0);
            }
            Err(e) => {
                // For remote commands: exit rather than reconnect.  The command
                // has finished (or the server is gone), so there is nothing to
                // reconnect to.  A full-screen command (btop, …) may have been
                // left holding the alternate screen, so reset it too.
                if has_remote_command {
                    restore_terminal(true);
                    eprintln!("[etr] Session ended: {e}");
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
    // True when a remote command was given rather than an interactive shell.
    // Controls whether stdin reaching EOF ends the session — see the
    // wait-for-completion `select!` at the end of this function.
    has_remote_command: bool,
    // `-4`/`-6`: which family to try first when resolving the local targets of
    // `-R` reverse forwards, which are resolved on this side.
    addr_pref: AddrPref,
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
        // Set once stdin has hit EOF and the VEOF byte has been relayed, so the
        // second pass through the loop parks instead of sending it again.
        let mut eof_signalled = false;
        loop {
            let payload = {
                let mut rx = stdin_rx.lock().await;
                match rx.recv().await {
                    Some(p) => p,
                    // Stdin reached EOF while running a remote command.
                    //
                    // Returning here would drop `pty_send`, which *finishes* the
                    // client→server half of the PTY QUIC stream — the server
                    // reads that as the session ending and tears the connection
                    // down, so the command's output never comes back
                    // ("Session ended: connection lost").
                    //
                    // Half-close instead, in two steps:
                    //
                    // 1. Relay `VEOF` (0x04) once. The remote side is a PTY, and
                    //    a PTY cannot be half-closed the way ssh half-closes a
                    //    pipe — there is no out-of-band "stdin is done" to send.
                    //    What a real terminal does here is exactly this: Ctrl-D
                    //    at a line start makes the reader's `read()` return 0. So
                    //    `printf x | etr host cat` lets `cat` see EOF and exit,
                    //    instead of blocking forever on a stream nothing will
                    //    ever close. Commands that don't read stdin (`echo …`)
                    //    never dequeue the byte and are unaffected.
                    // 2. Park forever, holding `pty_send` open so the server keeps
                    //    the session alive until the command exits and sends its
                    //    Disconnect. Teardown aborts this task, which is what
                    //    finally drops the stream.
                    None if has_remote_command && !eof_signalled => {
                        vlog!(verbose, 2, "[etr] stdin EOF; relaying VEOF to remote");
                        eof_signalled = true;
                        vec![0x04]
                    }
                    None if has_remote_command => {
                        vlog!(verbose, 2, "[etr] stdin EOF; holding PTY stream open");
                        std::future::pending::<()>().await;
                        unreachable!("pending() never resolves");
                    }
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

    #[cfg(unix)]
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

    // Windows has no SIGWINCH; poll the console size instead of waiting on a signal.
    #[cfg(windows)]
    let mut sigwinch_task = tokio::spawn(async move {
        let mut last = crossterm::terminal::size().ok();
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if let Ok(size) = crossterm::terminal::size()
                && Some(size) != last
            {
                last = Some(size);
                let (cols, rows) = size;
                let _ = resize_tx.try_send(TerminalResize {
                    rows: rows as u32,
                    cols: cols as u32,
                });
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
                    #[cfg(unix)]
                    {
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
                    }
                    // X11 forwarding is unsupported on Windows; -X/-Y is rejected at
                    // startup, so the server should never open an X11 stream here.
                    #[cfg(windows)]
                    let _ = quic_send.finish();
                    return;
                }
                let proto = ForwardProto::try_from(so.forward_proto).unwrap_or(ForwardProto::Tcp);
                match proto {
                    ForwardProto::Tcp => {
                        let addr = format!("{}:{}", so.remote_host, so.remote_port);
                        vlog!(verbose, 2, "[etr] connecting to local TCP target {addr}");
                        let tcp = match etr::forward::connect_tcp_preferred(&addr, addr_pref).await
                        {
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
                            match etr::forward::resolve_udp_target(&addr_str, addr_pref).await {
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
    //
    // The `stdin_task` arm is **disabled while a remote command is running**.
    // `stdin_task` finishes when the stdin channel closes, which happens on
    // stdin EOF — and with `etr host 'cmd' </dev/null` or a pipe, that EOF
    // arrives immediately, before the command has produced anything. Letting it
    // end the session aborted `pty_recv_task` and `stdout_task` along with it,
    // so the command's output was discarded and `etr` exited 0 having printed
    // nothing but the terminal reset. ssh does not behave that way: it stops
    // sending stdin but keeps relaying output until the remote side is done.
    //
    // With the arm disabled the session ends on a *definite* terminator instead
    // — the server's `Disconnect` when the command exits (via `ctrl_reader_task`)
    // or the PTY stream closing (`pty_recv_task`) — rather than on a timeout.
    //
    // Interactive sessions keep the old behaviour deliberately. There the remote
    // side is a shell that never exits on its own, and nothing in the PTY stream
    // carries "stdin is finished" to it, so dropping this arm would turn a prompt
    // return into a hang. Console stdin never EOFs, so the arm simply never fires
    // in normal interactive use.
    let result = tokio::select! {
        r = &mut ctrl_reader_task => r.unwrap_or_else(|e| Err(io::Error::other(e.to_string()))),
        _ = &mut stdin_task, if !has_remote_command => Ok(()),
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

#[cfg(unix)]
enum X11Stream {
    Unix(tokio::net::UnixStream),
    Tcp(tokio::net::TcpStream),
}

#[cfg(unix)]
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

#[cfg(unix)]
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
    fn test_prefer_family_flags_default_to_auto() {
        let cli = Cli::try_parse_from(["etr", "host"]).unwrap();
        assert!(!cli.prefer_ipv4);
        assert!(!cli.prefer_ipv6);
        assert_eq!(
            AddrPref::from_flags(cli.prefer_ipv4, cli.prefer_ipv6),
            AddrPref::Auto,
            "no flag must leave every resolution path on its previous default"
        );
    }

    #[test]
    fn test_prefer_ipv4_short_and_long_flags() {
        for args in [
            ["etr", "-4", "host"].as_slice(),
            ["etr", "--prefer-ipv4", "host"].as_slice(),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(cli.prefer_ipv4, "{args:?}");
            assert_eq!(
                AddrPref::from_flags(cli.prefer_ipv4, cli.prefer_ipv6),
                AddrPref::Ipv4
            );
        }
    }

    #[test]
    fn test_prefer_ipv6_short_and_long_flags() {
        for args in [
            ["etr", "-6", "host"].as_slice(),
            ["etr", "--prefer-ipv6", "host"].as_slice(),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(cli.prefer_ipv6, "{args:?}");
            assert_eq!(
                AddrPref::from_flags(cli.prefer_ipv4, cli.prefer_ipv6),
                AddrPref::Ipv6
            );
        }
    }

    #[test]
    fn test_prefer_families_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["etr", "-4", "-6", "host"]).is_err());
    }

    /// `-4`/`-6` must not be swallowed by `trailing_var_arg` when they precede
    /// the target, and must still reach the remote command when they follow it.
    #[test]
    fn test_prefer_flag_before_target_is_not_a_remote_command_arg() {
        let cli = Cli::try_parse_from(["etr", "-6", "host", "ls", "-la"]).unwrap();
        assert!(cli.prefer_ipv6);
        assert_eq!(cli.target.as_deref(), Some("host"));
        assert_eq!(cli.command, vec!["ls", "-la"]);
    }

    #[test]
    fn test_cli_flag_beats_config_address_family() {
        // CLI first, config as the fallback — the precedence main() applies.
        assert_eq!(
            AddrPref::from_flags(true, false).or(AddrPref::from_config(Some("ipv6"))),
            AddrPref::Ipv4
        );
        // With no flag, the config value is what takes effect.
        assert_eq!(
            AddrPref::from_flags(false, false).or(AddrPref::from_config(Some("ipv6"))),
            AddrPref::Ipv6
        );
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
    fn test_term_reset_modes_covers_critical_modes() {
        // Regression guard: the cursor-safe reset emitted on every session exit
        // must undo the emulator modes a remote full-screen app leaves set, or
        // the local terminal is unusable after a hard drop / `~.` quit.  Bytes
        // are checked so an accidental edit that drops one is caught.
        let seq = TERM_RESET_MODES;
        let contains = |needle: &[u8]| seq.windows(needle.len()).any(|w| w == needle);
        // Disable every mouse-reporting mode (mouse wheel spewing escapes).
        assert!(contains(b"\x1b[?1000l"));
        assert!(contains(b"\x1b[?1002l"));
        assert!(contains(b"\x1b[?1003l"));
        assert!(contains(b"\x1b[?1006l"));
        // Disable bracketed paste.
        assert!(contains(b"\x1b[?2004l"));
        // Show the cursor again.
        assert!(contains(b"\x1b[?25h"));
        // Reset SGR attributes.
        assert!(seq.ends_with(b"\x1b[0m"));
        // The cursor-safe reset must NOT move the cursor: no alternate-screen
        // switch (`?1049l`), no scroll-region reset (`\x1b[r`), no full RIS.
        assert!(!contains(b"\x1b[?1049l"));
        assert!(!contains(b"\x1b[r"));
        assert!(!contains(b"\x1bc"));
    }

    #[test]
    fn test_term_reset_screen_leaves_alt_screen() {
        // The screen-restoring reset (unclean exits only) must leave the
        // alternate screen and reset the scroll region, but never clear
        // scrollback with a full RIS.
        let seq = TERM_RESET_SCREEN;
        let contains = |needle: &[u8]| seq.windows(needle.len()).any(|w| w == needle);
        assert!(contains(b"\x1b[?1049l"));
        assert!(contains(b"\x1b[r"));
        assert!(!contains(b"\x1bc"));
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
