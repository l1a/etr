// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(unix)]

use etr::protocol::{Envelope, Heartbeat, Payload, SessionOpen};
use etr::quic::{self, TAG_CONTROL, TAG_PTY};
use etr::sessions::{SessionStatus, list_sessions_in, process_is_live_etrs};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn etrs_bin() -> &'static str {
    env!("CARGO_BIN_EXE_etrs")
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

struct DaemonGuard {
    pid: u32,
    port: u16,
    cert: Vec<u8>,
    session_id: [u8; 16],
    passkey: String,
    base_dir: PathBuf,
    state_home: PathBuf,
    config_home: PathBuf,
    sessions_dir: PathBuf,
}

impl DaemonGuard {
    fn session_file(&self) -> PathBuf {
        self.sessions_dir.join(format!("{}.toml", self.pid))
    }

    fn wait_for_exit(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !process_is_live_etrs(self.pid) && !self.session_file().exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        !process_is_live_etrs(self.pid) && !self.session_file().exists()
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if process_is_live_etrs(self.pid) {
            let pid = Pid::from_raw(self.pid as i32);
            let _ = kill(pid, Signal::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && process_is_live_etrs(self.pid) {
                std::thread::sleep(Duration::from_millis(25));
            }
            if process_is_live_etrs(self.pid) {
                let _ = kill(pid, Signal::SIGKILL);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        std::fs::remove_dir_all(&self.base_dir).ok();
    }
}

fn spawn_etrs(args: &[&str], extra_bootstrap_lines: &[&str]) -> DaemonGuard {
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let base_dir = etr::config::fallback_tmp_state_dir()
        .join(format!("it-timeouts-{}-{n}", std::process::id()));
    std::fs::remove_dir_all(&base_dir).ok();

    let state_home = base_dir.join("state");
    let config_home = base_dir.join("config");
    drop(etr::config::open_trusted_dir(&state_home).unwrap());
    drop(etr::config::open_trusted_dir(&config_home).unwrap());

    let session_id: [u8; 16] = rand::random();
    let passkey = "TestPasskey1234567890abcdefghijk".to_string();

    let mut cmd = Command::new(etrs_bin());
    cmd.args(["-b", "127.0.0.1"])
        .args(args)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env_remove("ETR_SERVER_NETWORK_TMOUT")
        .env_remove("ETR_SERVER_CONNECT_TMOUT")
        .env_remove("ETR_SERVER_SIGNAL_TMOUT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn etrs");
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(
            stdin,
            "{}/{passkey}/xterm-256color",
            hex_encode(&session_id)
        )
        .unwrap();
        writeln!(stdin, "ETRCMD:sleep 30").unwrap();
        for line in extra_bootstrap_lines {
            writeln!(stdin, "{line}").unwrap();
        }
        stdin.flush().unwrap();
    }

    let output = child.wait_with_output().expect("wait on parent etrs");
    assert!(
        output.status.success(),
        "etrs bootstrap failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut parsed = None;
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix("PORT ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 3
                && parts[1] == "CERT"
                && let Ok(port) = parts[0].parse::<u16>()
                && let Some(cert) = hex_decode(parts[2])
            {
                parsed = Some((port, cert));
                break;
            }
        }
    }
    let (port, cert) = parsed.expect("etrs did not print PORT/CERT");

    let sessions_dir = state_home.join("etr").join("sessions");
    let pid = wait_for_registered_pid(&sessions_dir, port, Duration::from_secs(3));

    DaemonGuard {
        pid,
        port,
        cert,
        session_id,
        passkey,
        base_dir,
        state_home,
        config_home,
        sessions_dir,
    }
}

fn wait_for_registered_pid(sessions_dir: &Path, port: u16, timeout: Duration) -> u32 {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(res) = list_sessions_in(sessions_dir)
            && let Some(rec) = res.sessions.into_iter().find(|r| r.port == port)
        {
            return rec.pid;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "timed out waiting for etrs session record for port {port} in {}",
        sessions_dir.display()
    );
}

#[test]
fn test_initial_connect_timeout_expires_and_cleans_registry() {
    let guard = spawn_etrs(&["--connect-timeout", "1"], &[]);
    let res = list_sessions_in(&guard.sessions_dir).unwrap();
    assert_eq!(res.sessions.len(), 1);
    assert_eq!(res.sessions[0].pid, guard.pid);
    assert_eq!(res.sessions[0].state, SessionStatus::Waiting);

    assert!(
        guard.wait_for_exit(Duration::from_secs(5)),
        "etrs daemon {} should exit after 1s initial connect timeout",
        guard.pid
    );
    assert!(!guard.session_file().exists());
}

#[test]
fn test_client_bootstrap_connect_timeout_overrides_default() {
    let guard = spawn_etrs(&[], &["ETRCONNECTTIMEOUT:1"]);
    assert!(
        guard.wait_for_exit(Duration::from_secs(5)),
        "client ETRCONNECTTIMEOUT:1 should override 60s default and exit"
    );
    assert!(!guard.session_file().exists());
}

#[test]
fn test_sigusr1_reaps_disconnected_session_and_honours_signal_timeout() {
    // 1. Default signal_timeout=0: SIGUSR1 reaps waiting/disconnected session promptly.
    let guard_zero = spawn_etrs(&["--connect-timeout", "60", "--signal-timeout", "0"], &[]);
    kill(Pid::from_raw(guard_zero.pid as i32), Signal::SIGUSR1).unwrap();
    assert!(
        guard_zero.wait_for_exit(Duration::from_secs(3)),
        "SIGUSR1 with signal_timeout=0 should terminate waiting daemon"
    );

    // 2. signal_timeout=300: SIGUSR1 is ignored while disconnected < 300s.
    let guard_delay = spawn_etrs(&["--connect-timeout", "60", "--signal-timeout", "300"], &[]);
    kill(Pid::from_raw(guard_delay.pid as i32), Signal::SIGUSR1).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        process_is_live_etrs(guard_delay.pid),
        "SIGUSR1 must be ignored when elapsed < signal_timeout"
    );
    assert!(guard_delay.session_file().exists());
}

#[test]
fn test_etrs_list_and_clean_with_older_than() {
    let guard = spawn_etrs(&["--connect-timeout", "60", "--signal-timeout", "0"], &[]);

    let list_out = Command::new(etrs_bin())
        .arg("--list")
        .env("XDG_STATE_HOME", &guard.state_home)
        .env("XDG_CONFIG_HOME", &guard.config_home)
        .output()
        .unwrap();
    assert!(list_out.status.success());
    let list_stdout = String::from_utf8_lossy(&list_out.stdout);
    assert!(
        list_stdout.contains(&guard.pid.to_string()),
        "etrs --list output missing pid {}: {list_stdout}",
        guard.pid
    );
    assert!(
        list_stdout.contains("waiting"),
        "etrs --list output missing waiting state: {list_stdout}"
    );

    // --clean --older-than 300 must skip a freshly started session.
    let clean_skip = Command::new(etrs_bin())
        .args(["--clean", "--older-than", "300"])
        .env("XDG_STATE_HOME", &guard.state_home)
        .env("XDG_CONFIG_HOME", &guard.config_home)
        .output()
        .unwrap();
    assert!(clean_skip.status.success());
    assert!(
        process_is_live_etrs(guard.pid),
        "--clean --older-than 300 must not reap a 0s-old session"
    );

    // --clean without --older-than reaps the waiting session via SIGUSR1.
    let clean_now = Command::new(etrs_bin())
        .arg("--clean")
        .env("XDG_STATE_HOME", &guard.state_home)
        .env("XDG_CONFIG_HOME", &guard.config_home)
        .output()
        .unwrap();
    assert!(clean_now.status.success());
    assert!(
        guard.wait_for_exit(Duration::from_secs(3)),
        "--clean should reap waiting daemon"
    );

    let list_after = Command::new(etrs_bin())
        .arg("--list")
        .env("XDG_STATE_HOME", &guard.state_home)
        .env("XDG_CONFIG_HOME", &guard.config_home)
        .output()
        .unwrap();
    assert!(list_after.status.success());
    let after_stdout = String::from_utf8_lossy(&list_after.stdout);
    assert!(
        after_stdout.contains("No active etrs sessions"),
        "unexpected post-clean --list output: {after_stdout}"
    );
}

#[tokio::test]
async fn test_authenticated_session_ignores_sigusr1_and_uses_reconnect_timeout() {
    let guard = spawn_etrs(&[], &["ETRCONNECTTIMEOUT:60", "ETRRECONNECTTIMEOUT:2"]);

    let cert = rustls::pki_types::CertificateDer::from(guard.cert.clone());
    let cli_cfg = quic::client_config(cert).unwrap();
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(cli_cfg);

    let addr: SocketAddr = ([127, 0, 0, 1], guard.port).into();
    let conn = endpoint
        .connect(addr, "etr")
        .unwrap()
        .await
        .expect("QUIC connect");

    let (mut ctrl_send, mut ctrl_recv) = conn.open_bi().await.unwrap();
    ctrl_send.write_all(&[TAG_CONTROL]).await.unwrap();
    let open = Envelope {
        payload: Some(Payload::SessionOpen(SessionOpen {
            session_id: guard.session_id.to_vec(),
            passkey: guard.passkey.clone(),
            last_received_seq: Default::default(),
            reverse_forwards: vec![],
            gateway_ports: false,
            x11_enabled: false,
            x11_auth_proto: String::new(),
            x11_auth_cookie: vec![],
        })),
    };
    quic::write_msg(&mut ctrl_send, &open).await.unwrap();

    let accept = quic::read_msg(&mut ctrl_recv).await.unwrap().unwrap();
    assert!(matches!(accept.payload, Some(Payload::SessionAccept(_))));

    let (mut pty_send, _pty_recv) = conn.open_bi().await.unwrap();
    pty_send.write_all(&[TAG_PTY]).await.unwrap();

    // Wait for registry to transition to Connected.
    let mut connected_seen = false;
    for _ in 0..80 {
        let res = list_sessions_in(&guard.sessions_dir).unwrap();
        if let Some(rec) = res.sessions.first()
            && rec.state == SessionStatus::Connected
        {
            connected_seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(connected_seen, "session did not transition to Connected");

    // SIGUSR1 delivered while connected must be ignored (and drained).
    kill(Pid::from_raw(guard.pid as i32), Signal::SIGUSR1).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        process_is_live_etrs(guard.pid),
        "connected session must ignore SIGUSR1"
    );

    // Control stream must still accept a Heartbeat.
    let hb = Envelope {
        payload: Some(Payload::Heartbeat(Heartbeat::default())),
    };
    quic::write_msg(&mut ctrl_send, &hb).await.unwrap();

    // Abruptly close the QUIC connection (without Disconnect) so etrs enters
    // the reconnect loop.
    conn.close(quinn::VarInt::from_u32(0), b"simulated drop");
    drop((ctrl_send, ctrl_recv, pty_send, _pty_recv, conn));
    endpoint.wait_idle().await;

    // Verify transition to Disconnected — proving the earlier SIGUSR1 was
    // drained and did NOT kill the session immediately on disconnect.
    let mut disconnected_seen = false;
    for _ in 0..60 {
        let res = list_sessions_in(&guard.sessions_dir).unwrap();
        if let Some(rec) = res.sessions.first()
            && rec.state == SessionStatus::Disconnected
        {
            disconnected_seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        disconnected_seen,
        "session should enter Disconnected state after drop rather than being killed by earlier SIGUSR1"
    );

    // After the 2s ETRRECONNECTTIMEOUT window expires, etrs exits cleanly.
    assert!(
        guard.wait_for_exit(Duration::from_secs(5)),
        "daemon {} should exit after 2s reconnect timeout",
        guard.pid
    );
    assert!(!guard.session_file().exists());
}
