// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-user session registry for inspecting (`etrs --list`) and safely
//! cleaning up disconnected sessions (`etrs --clean`).
//!
//! Each running `etrs` daemon writes a `0600` TOML record under
//! `$XDG_STATE_HOME/etr/sessions/<pid>.toml` (mode `0700` directory) using an
//! exclusive temp file and atomic rename, and removes it on exit.  Sensitive
//! credentials (`passkey`, full `session_id`, raw command arguments) are never
//! stored.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Lifecycle state of an `etrs` daemon process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    /// Bootstrapped via SSH, waiting for the client's first authenticated connection.
    Waiting,
    /// Actively serving an authenticated QUIC connection.
    Connected,
    /// Previously connected; client dropped and daemon is waiting for reconnect.
    Disconnected,
}

impl SessionStatus {
    /// Lowercase human-readable label.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Waiting => "waiting",
            SessionStatus::Connected => "connected",
            SessionStatus::Disconnected => "disconnected",
        }
    }
}

/// Whether the session hosts an interactive shell or a single remote command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    Shell,
    Command,
}

impl SessionMode {
    /// Lowercase human-readable label.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionMode::Shell => "shell",
            SessionMode::Command => "command",
        }
    }
}

/// Non-sensitive metadata written by an `etrs` daemon to `<pid>.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub pid: u32,
    pub port: u16,
    pub started_at: u64,
    pub state: SessionStatus,
    pub peer: Option<String>,
    pub disconnected_at: Option<u64>,
    pub reconnect_timeout: u64,
    pub connect_timeout: u64,
    pub mode: SessionMode,
}

/// Return the directory where per-session metadata files are stored:
/// `$XDG_STATE_HOME/etr/sessions`.
pub fn sessions_dir() -> PathBuf {
    crate::config::state_dir().join("sessions")
}

/// Current wall-clock timestamp in Unix seconds.
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Return whether a session record is eligible for `etrs --clean` at `now_secs`.
///
/// Connected sessions are never eligible.  Waiting and disconnected sessions
/// are eligible once they have been waiting/disconnected for at least
/// `older_than_secs`.
pub fn is_eligible_for_clean(record: &SessionRecord, now_secs: u64, older_than_secs: u64) -> bool {
    match record.state {
        SessionStatus::Connected => false,
        SessionStatus::Waiting | SessionStatus::Disconnected => {
            let since = record.disconnected_at.unwrap_or(record.started_at);
            now_secs.saturating_sub(since) >= older_than_secs
        }
    }
}

/// Format a duration in seconds into a compact human-readable string
/// (e.g. `"45s"`, `"12m 5s"`, `"3h 20m"`, `"4d 0h"`).
pub fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        }
    } else if secs < 86_400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    } else {
        let d = secs / 86_400;
        let h = (secs % 86_400) / 3600;
        format!("{d}d {h}h")
    }
}

/// Format a configured timeout value where `0` means `"forever"`.
pub fn format_timeout(secs: u64) -> String {
    if secs == 0 {
        "forever".to_string()
    } else {
        format_duration(secs)
    }
}

/// Column headers printed by `etrs --list`.
pub const LIST_HEADERS: [&str; 8] = [
    "PID", "STATE", "PORT", "MODE", "UPTIME", "PEER", "IDLE", "TIMEOUT",
];

/// Format session records as an aligned table with dynamic column widths.
///
/// Returns an empty string when `sessions` is empty so callers can handle
/// empty-state messaging separately.  Each column widens to fit its header and
/// widest cell with a 2-space gutter, and the final column has no trailing
/// padding.
pub fn format_session_table(sessions: &[SessionRecord], now: u64) -> String {
    if sessions.is_empty() {
        return String::new();
    }

    let rows: Vec<[String; 8]> = sessions
        .iter()
        .map(|s| {
            let uptime = format_duration(now.saturating_sub(s.started_at));
            let idle = match s.state {
                SessionStatus::Connected => "-".to_string(),
                SessionStatus::Waiting | SessionStatus::Disconnected => {
                    format_duration(now.saturating_sub(s.disconnected_at.unwrap_or(s.started_at)))
                }
            };
            let timeout = match s.state {
                SessionStatus::Waiting => format_timeout(s.connect_timeout),
                SessionStatus::Connected | SessionStatus::Disconnected => {
                    format_timeout(s.reconnect_timeout)
                }
            };
            [
                s.pid.to_string(),
                s.state.as_str().to_string(),
                s.port.to_string(),
                s.mode.as_str().to_string(),
                uptime,
                s.peer.as_deref().unwrap_or("-").to_string(),
                idle,
                timeout,
            ]
        })
        .collect();

    let mut widths: [usize; 8] = LIST_HEADERS.map(str::len);
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row.iter()) {
            *w = (*w).max(cell.chars().count());
        }
    }

    let mut out = String::new();
    append_table_row(&mut out, &LIST_HEADERS, &widths);
    for row in &rows {
        let cells = row.each_ref().map(String::as_str);
        append_table_row(&mut out, &cells, &widths);
    }
    out
}

fn append_table_row(out: &mut String, cells: &[&str; 8], widths: &[usize; 8]) {
    use std::fmt::Write;

    if let Some((last, init)) = cells.split_last() {
        for (cell, width) in init.iter().zip(widths.iter()) {
            let _ = write!(out, "{cell:<width$}  ");
        }
        let _ = writeln!(out, "{last}");
    }
}

#[cfg(unix)]
mod unix_imp {
    use super::*;
    use nix::fcntl::{OFlag, openat, renameat};
    use nix::sys::signal::{Signal, kill};
    use nix::sys::stat::Mode;
    use nix::unistd::{Pid, UnlinkatFlags, geteuid, unlinkat};
    use std::io::{self, Read, Write};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;

    fn write_record_atomic(dir: &Path, record: &SessionRecord, seq: u64) -> io::Result<()> {
        let dir_file = crate::config::open_trusted_dir(dir)?;
        let body = toml::to_string(record).map_err(io::Error::other)?;
        let nonce: u32 = rand::random();
        let tmp_name = format!(".tmp-{}-{seq}-{nonce:08x}", record.pid);
        let final_name = format!("{}.toml", record.pid);

        let tmp_fd = openat(
            &dir_file,
            tmp_name.as_str(),
            OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_WRONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(io::Error::from)?;
        let mut tmp_file = std::fs::File::from(tmp_fd);

        if let Err(e) = tmp_file
            .write_all(body.as_bytes())
            .and_then(|()| tmp_file.sync_data().or(Ok(())))
        {
            drop(tmp_file);
            let _ = unlinkat(&dir_file, tmp_name.as_str(), UnlinkatFlags::NoRemoveDir);
            return Err(e);
        }
        let _ = tmp_file.set_permissions(std::fs::Permissions::from_mode(0o600));
        drop(tmp_file);

        if let Err(e) = renameat(&dir_file, tmp_name.as_str(), &dir_file, final_name.as_str()) {
            let _ = unlinkat(&dir_file, tmp_name.as_str(), UnlinkatFlags::NoRemoveDir);
            return Err(io::Error::from(e));
        }
        Ok(())
    }

    /// RAII guard that keeps `<pid>.toml` updated across connection state
    /// transitions and removes it when the daemon shuts down.
    pub struct SessionRegistryGuard {
        dir: PathBuf,
        record: SessionRecord,
        write_seq: u64,
    }

    impl SessionRegistryGuard {
        /// Atomically register `record` inside `dir` and return a guard that
        /// deletes `<pid>.toml` on drop.
        pub fn register(dir: PathBuf, record: SessionRecord) -> io::Result<Self> {
            write_record_atomic(&dir, &record, 0)?;
            Ok(Self {
                dir,
                record,
                write_seq: 0,
            })
        }

        /// Record that an authenticated client connection is currently active.
        pub fn set_connected(&mut self, peer: String) {
            self.record.state = SessionStatus::Connected;
            self.record.peer = Some(peer);
            self.record.disconnected_at = None;
            self.write_seq += 1;
            let _ = write_record_atomic(&self.dir, &self.record, self.write_seq);
        }

        /// Record that the active client dropped and the daemon is waiting for
        /// reconnection.
        pub fn set_disconnected(&mut self) {
            self.record.state = SessionStatus::Disconnected;
            self.record.disconnected_at = Some(now_unix_secs());
            self.write_seq += 1;
            let _ = write_record_atomic(&self.dir, &self.record, self.write_seq);
        }
    }

    impl Drop for SessionRegistryGuard {
        fn drop(&mut self) {
            if let Ok(dir_file) = crate::config::open_trusted_dir(&self.dir) {
                let final_name = format!("{}.toml", self.record.pid);
                let _ = unlinkat(&dir_file, final_name.as_str(), UnlinkatFlags::NoRemoveDir);
            }
        }
    }

    /// Return `true` only if `pid` refers to a live process owned by the
    /// current effective UID whose executable name is `etrs`.
    ///
    /// Guards against signalling recycled PIDs belonging to unrelated processes.
    pub fn process_is_live_etrs(pid: u32) -> bool {
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        if kill(Pid::from_raw(pid as i32), None).is_err() {
            return false;
        }

        #[cfg(target_os = "linux")]
        {
            let proc_dir = format!("/proc/{pid}");
            let Ok(meta) = std::fs::metadata(&proc_dir) else {
                return false;
            };
            if meta.uid() != geteuid().as_raw() {
                return false;
            }
            let Ok(comm) = std::fs::read_to_string(format!("{proc_dir}/comm")) else {
                return false;
            };
            if comm.trim() != "etrs" {
                return false;
            }
            // `kill(pid, 0)` and `/proc/<pid>/comm` both still report an exited
            // zombie (`Z`) as present (e.g. under a container/sandbox PID 1 that
            // does not immediately reap reparented grandchildren).
            let Ok(stat) = std::fs::read_to_string(format!("{proc_dir}/stat")) else {
                return false;
            };
            let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start()) else {
                return false;
            };
            !matches!(after_comm.chars().next(), Some('Z' | 'X'))
        }

        #[cfg(not(target_os = "linux"))]
        {
            let Ok(output) = std::process::Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "uid=,state=,comm="])
                .output()
            else {
                return false;
            };
            if !output.status.success() {
                return false;
            }
            let text = String::from_utf8_lossy(&output.stdout);
            let Some(line) = text.lines().next().map(str::trim).filter(|l| !l.is_empty()) else {
                return false;
            };
            let Some((uid_str, after_uid)) = line.split_once(char::is_whitespace) else {
                return false;
            };
            let Some((state_str, after_state)) =
                after_uid.trim_start().split_once(char::is_whitespace)
            else {
                return false;
            };
            let comm_str = after_state.trim();
            let Ok(uid) = uid_str.parse::<u32>() else {
                return false;
            };
            if uid != geteuid().as_raw() {
                return false;
            }
            if state_str.starts_with('Z') || state_str.starts_with('X') {
                return false;
            }
            Path::new(comm_str).file_name().and_then(|n| n.to_str()) == Some("etrs")
        }
    }

    /// Output from [`list_sessions_in`].
    #[derive(Debug, Default)]
    pub struct ListResult {
        pub sessions: Vec<SessionRecord>,
        pub pruned_stale: usize,
    }

    /// List live `etrs` sessions recorded under `dir`, auto-pruning stale files
    /// (dead PIDs, reused non-`etrs` PIDs, corrupt/mismatched records).
    pub fn list_sessions_in(dir: &Path) -> io::Result<ListResult> {
        if let Err(e) = std::fs::symlink_metadata(dir)
            && e.kind() == io::ErrorKind::NotFound
        {
            return Ok(ListResult::default());
        }

        let dir_file = crate::config::open_trusted_dir(dir)?;
        let euid = geteuid().as_raw();
        let mut sessions = Vec::new();
        let mut pruned_stale = 0usize;

        for entry in std::fs::read_dir(dir)? {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let file_name = entry.file_name();
            let Some(name_str) = file_name.to_str() else {
                continue;
            };
            if name_str.starts_with('.') || !name_str.ends_with(".toml") {
                continue;
            }
            let stem = &name_str[..name_str.len() - 5];

            let fd = match openat(
                &dir_file,
                name_str,
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(nix::errno::Errno::ENOENT) => continue,
                Err(nix::errno::Errno::ELOOP) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("refusing symlinked session file {name_str}"),
                    ));
                }
                Err(_) => continue,
            };
            let mut file = std::fs::File::from(fd);
            let Ok(meta) = file.metadata() else {
                continue;
            };
            if !meta.is_file() || meta.uid() != euid {
                continue;
            }

            let mut content = String::new();
            let valid_live = if file.read_to_string(&mut content).is_ok()
                && let Ok(record) = toml::from_str::<SessionRecord>(&content)
                && stem == record.pid.to_string()
                && process_is_live_etrs(record.pid)
            {
                sessions.push(record);
                true
            } else {
                false
            };
            drop(file);

            if !valid_live && unlinkat(&dir_file, name_str, UnlinkatFlags::NoRemoveDir).is_ok() {
                pruned_stale += 1;
            }
        }

        sessions.sort_by_key(|r| (r.started_at, r.pid));
        Ok(ListResult {
            sessions,
            pruned_stale,
        })
    }

    /// Summary returned by [`clean_sessions_in`].
    #[derive(Debug, Default)]
    pub struct CleanReport {
        pub signalled: Vec<u32>,
        pub exited: Vec<u32>,
        pub still_running: Vec<u32>,
        pub skipped_connected: usize,
        pub skipped_recent: usize,
        pub pruned_stale: usize,
    }

    /// Safely terminate disconnected/waiting `etrs` sessions under `dir` using
    /// `SIGUSR1` (which connected sessions ignore even in a TOCTOU race).
    pub fn clean_sessions_in(dir: &Path, older_than_secs: u64) -> io::Result<CleanReport> {
        let list = list_sessions_in(dir)?;
        let now = now_unix_secs();
        let mut report = CleanReport {
            pruned_stale: list.pruned_stale,
            ..CleanReport::default()
        };

        for record in list.sessions {
            if record.state == SessionStatus::Connected {
                report.skipped_connected += 1;
                continue;
            }
            if !is_eligible_for_clean(&record, now, older_than_secs) {
                report.skipped_recent += 1;
                continue;
            }
            match kill(Pid::from_raw(record.pid as i32), Signal::SIGUSR1) {
                Ok(()) => report.signalled.push(record.pid),
                Err(nix::errno::Errno::ESRCH) => {}
                Err(e) => return Err(io::Error::from(e)),
            }
        }

        // Exceed utempter logout + hangup_child's 1 s SIGHUP-to-SIGKILL escalation
        // so cleanly terminating daemons are reported as exited, while fast
        // exits still return immediately on the first 50 ms tick.
        const CLEAN_WAIT_TICKS: usize = 60;
        const CLEAN_WAIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

        if !report.signalled.is_empty() {
            for _ in 0..CLEAN_WAIT_TICKS {
                if report
                    .signalled
                    .iter()
                    .all(|&pid| !process_is_live_etrs(pid))
                {
                    break;
                }
                std::thread::sleep(CLEAN_WAIT_INTERVAL);
            }
            for &pid in &report.signalled {
                if process_is_live_etrs(pid) {
                    report.still_running.push(pid);
                } else {
                    report.exited.push(pid);
                }
            }
        }

        Ok(report)
    }
}

#[cfg(unix)]
pub use unix_imp::{
    CleanReport, ListResult, SessionRegistryGuard, clean_sessions_in, list_sessions_in,
    process_is_live_etrs,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration_and_timeout() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(60), "1m");
        assert_eq!(format_duration(725), "12m 5s");
        assert_eq!(format_duration(3600), "1h");
        assert_eq!(format_duration(3 * 3600 + 20 * 60), "3h 20m");
        assert_eq!(format_duration(345_600), "4d 0h");
        assert_eq!(format_duration(86_400 + 5 * 3600), "1d 5h");

        assert_eq!(format_timeout(0), "forever");
        assert_eq!(format_timeout(60), "1m");
        assert_eq!(format_timeout(345_600), "4d 0h");
    }

    #[test]
    fn test_is_eligible_for_clean() {
        let base = SessionRecord {
            pid: 1234,
            port: 40000,
            started_at: 1000,
            state: SessionStatus::Connected,
            peer: Some("127.0.0.1".to_string()),
            disconnected_at: None,
            reconnect_timeout: 345_600,
            connect_timeout: 60,
            mode: SessionMode::Shell,
        };
        assert!(!is_eligible_for_clean(&base, 10_000, 0));
        assert!(!is_eligible_for_clean(&base, 10_000, 60));

        let waiting = SessionRecord {
            state: SessionStatus::Waiting,
            peer: None,
            disconnected_at: Some(1000),
            ..base.clone()
        };
        assert!(is_eligible_for_clean(&waiting, 1000, 0));
        assert!(!is_eligible_for_clean(&waiting, 1059, 60));
        assert!(is_eligible_for_clean(&waiting, 1060, 60));

        let disconnected = SessionRecord {
            state: SessionStatus::Disconnected,
            disconnected_at: Some(2000),
            ..base
        };
        assert!(!is_eligible_for_clean(&disconnected, 2099, 100));
        assert!(is_eligible_for_clean(&disconnected, 2100, 100));
        assert!(is_eligible_for_clean(&disconnected, 2000, 0));
    }

    #[test]
    fn test_session_record_toml_round_trip() {
        for (state, peer, disc, mode) in [
            (SessionStatus::Waiting, None, Some(100), SessionMode::Shell),
            (
                SessionStatus::Connected,
                Some("2001:db8::1".to_string()),
                None,
                SessionMode::Command,
            ),
            (
                SessionStatus::Disconnected,
                Some("192.0.2.1".to_string()),
                Some(200),
                SessionMode::Shell,
            ),
        ] {
            let rec = SessionRecord {
                pid: 4242,
                port: 54321,
                started_at: 100,
                state,
                peer,
                disconnected_at: disc,
                reconnect_timeout: 345_600,
                connect_timeout: 60,
                mode,
            };
            let s = toml::to_string(&rec).unwrap();
            let decoded: SessionRecord = toml::from_str(&s).unwrap();
            assert_eq!(decoded, rec);
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_registry_guard_lifecycle_and_security() {
        use std::os::unix::fs::PermissionsExt;

        let base = crate::config::fallback_tmp_state_dir()
            .join(format!("sessions-unit-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        // Ensure parent base exists with 0700 under sticky /tmp before open_trusted_dir runs,
        // or let open_trusted_dir create both `base` and `sessions` with 0700.
        let dir = base.join("sessions");

        let pid = std::process::id();
        let rec = SessionRecord {
            pid,
            port: 12345,
            started_at: 1000,
            state: SessionStatus::Waiting,
            peer: None,
            disconnected_at: Some(1000),
            reconnect_timeout: 345_600,
            connect_timeout: 60,
            mode: SessionMode::Shell,
        };
        let mut guard = SessionRegistryGuard::register(dir.clone(), rec).unwrap();

        let dir_meta = std::fs::symlink_metadata(&dir).unwrap();
        assert_eq!(dir_meta.permissions().mode() & 0o777, 0o700);
        let file_path = dir.join(format!("{pid}.toml"));
        let file_meta = std::fs::symlink_metadata(&file_path).unwrap();
        assert_eq!(file_meta.permissions().mode() & 0o777, 0o600);

        guard.set_connected("127.0.0.1".to_string());
        let parsed: SessionRecord =
            toml::from_str(&std::fs::read_to_string(&file_path).unwrap()).unwrap();
        assert_eq!(parsed.state, SessionStatus::Connected);
        assert_eq!(parsed.peer.as_deref(), Some("127.0.0.1"));
        assert!(parsed.disconnected_at.is_none());

        guard.set_disconnected();
        let parsed: SessionRecord =
            toml::from_str(&std::fs::read_to_string(&file_path).unwrap()).unwrap();
        assert_eq!(parsed.state, SessionStatus::Disconnected);
        assert!(parsed.disconnected_at.is_some());

        drop(guard);
        assert!(
            !file_path.exists(),
            "SessionRegistryGuard drop must remove <pid>.toml"
        );

        // Symlinked directory must be rejected.
        let real_dir = base.join("real_sessions");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::set_permissions(&real_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let sym_dir = base.join("sym_sessions");
        std::os::unix::fs::symlink(&real_dir, &sym_dir).unwrap();
        let bad_rec = SessionRecord {
            pid: 999_999_999,
            port: 1,
            started_at: 1,
            state: SessionStatus::Waiting,
            peer: None,
            disconnected_at: Some(1),
            reconnect_timeout: 60,
            connect_timeout: 60,
            mode: SessionMode::Shell,
        };
        assert!(SessionRegistryGuard::register(sym_dir.clone(), bad_rec.clone()).is_err());
        assert!(list_sessions_in(&sym_dir).is_err());

        // Symlinked file inside sessions dir must be rejected by list_sessions_in.
        let target_file = base.join("decoy.toml");
        std::fs::write(&target_file, toml::to_string(&bad_rec).unwrap()).unwrap();
        std::os::unix::fs::symlink(&target_file, dir.join("999999999.toml")).unwrap();
        assert!(list_sessions_in(&dir).is_err());
        std::fs::remove_file(dir.join("999999999.toml")).unwrap();

        // Stale dead PID and live non-etrs PID (our own test PID) must be auto-pruned
        // without signalling them.
        let dead_file = dir.join("999999998.toml");
        let dead_rec = SessionRecord {
            pid: 999_999_998,
            ..bad_rec
        };
        std::fs::write(&dead_file, toml::to_string(&dead_rec).unwrap()).unwrap();

        let self_file = dir.join(format!("{pid}.toml"));
        let self_rec = SessionRecord { pid, ..dead_rec };
        std::fs::write(&self_file, toml::to_string(&self_rec).unwrap()).unwrap();

        let res = list_sessions_in(&dir).unwrap();
        assert!(res.sessions.is_empty());
        assert_eq!(res.pruned_stale, 2);
        assert!(!dead_file.exists());
        assert!(!self_file.exists());

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn test_format_session_table_aligns_ipv6_and_mixed_peers() {
        let now = 10_000;
        let ipv6_peer = "2001:db8:85a3:1234:5678:8a2e:370:7334";
        let records = vec![
            SessionRecord {
                pid: 82193,
                port: 47064,
                started_at: now - (17 * 60 + 13),
                state: SessionStatus::Disconnected,
                peer: Some(ipv6_peer.to_string()),
                disconnected_at: Some(now - (3 * 60 + 50)),
                reconnect_timeout: 345_600,
                connect_timeout: 60,
                mode: SessionMode::Shell,
            },
            SessionRecord {
                pid: 84497,
                port: 36376,
                started_at: now - 95,
                state: SessionStatus::Connected,
                peer: Some("192.0.2.10".to_string()),
                disconnected_at: None,
                reconnect_timeout: 345_600,
                connect_timeout: 60,
                mode: SessionMode::Shell,
            },
            SessionRecord {
                pid: 900,
                port: 50000,
                started_at: now - 5,
                state: SessionStatus::Waiting,
                peer: None,
                disconnected_at: Some(now - 5),
                reconnect_timeout: 0,
                connect_timeout: 60,
                mode: SessionMode::Command,
            },
        ];

        let table = format_session_table(&records, now);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 4);
        for line in &lines {
            assert_eq!(
                *line,
                line.trim_end(),
                "unexpected trailing spaces: {line:?}"
            );
        }

        let header = lines[0];
        let offsets: Vec<usize> = LIST_HEADERS
            .iter()
            .map(|h| header.find(h).unwrap())
            .collect();

        let expected_rows = [
            [
                "82193",
                "disconnected",
                "47064",
                "shell",
                "17m 13s",
                ipv6_peer,
                "3m 50s",
                "4d 0h",
            ],
            [
                "84497",
                "connected",
                "36376",
                "shell",
                "1m 35s",
                "192.0.2.10",
                "-",
                "4d 0h",
            ],
            ["900", "waiting", "50000", "command", "5s", "-", "5s", "1m"],
        ];

        for (line, expected) in lines[1..].iter().zip(expected_rows.iter()) {
            for (idx, (&offset, cell)) in offsets.iter().zip(expected.iter()).enumerate() {
                assert!(
                    line[offset..].starts_with(cell),
                    "column {idx} ({}) misaligned in {line:?}: expected {cell:?} at {offset}",
                    LIST_HEADERS[idx]
                );
                if idx > 0 {
                    assert_eq!(&line[offset - 2..offset], "  ");
                }
            }
        }
    }

    #[test]
    fn test_format_session_table_empty_and_wide_columns() {
        assert_eq!(format_session_table(&[], 1000), "");

        let now = 20_000_000;
        let records = vec![SessionRecord {
            pid: 1_234_567,
            port: 65535,
            started_at: now - (120 * 86_400 + 5 * 3600),
            state: SessionStatus::Connected,
            peer: Some("2001:db8::1".to_string()),
            disconnected_at: None,
            reconnect_timeout: 0,
            connect_timeout: 0,
            mode: SessionMode::Command,
        }];

        let table = format_session_table(&records, now);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], lines[0].trim_end());
        assert_eq!(lines[1], lines[1].trim_end());

        let header = lines[0];
        let row = lines[1];
        for (h, val) in LIST_HEADERS.iter().zip([
            "1234567",
            "connected",
            "65535",
            "command",
            "120d 5h",
            "2001:db8::1",
            "-",
            "forever",
        ]) {
            let offset = header.find(h).unwrap();
            assert!(
                row[offset..].starts_with(val),
                "header {h} at {offset} does not match {val:?} in {row:?}"
            );
        }
    }
}
