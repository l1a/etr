// SPDX-License-Identifier: GPL-3.0-or-later
//
// utmp/wtmp login record management.
//
// Writes USER_PROCESS on session start and DEAD_PROCESS on session end,
// matching what ssh and mosh do so the session appears in `who`, `w`, and `last`.
//
// Only compiled on Linux — other platforms get no-op stubs.

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::CString;
    use std::net::IpAddr;
    use std::path::Path;
    use utmpx::sys::{UtType, Utmpx};

    // libc doesn't expose updwtmpx for Linux; declare it directly.
    unsafe extern "C" {
        fn updwtmpx(file: *const libc::c_char, ut: *const Utmpx) -> libc::c_int;
    }

    /// Write a USER_PROCESS utmp entry and append to wtmp.
    /// `tty_path` is the full PTY slave path (e.g. `/dev/pts/3`).
    /// `remote_addr` is the connecting client's IP string.
    pub fn record_login(pid: i32, tty_path: &Path, remote_addr: &str) {
        let Some((ut_line, ut_id)) = tty_ids(tty_path) else {
            return;
        };
        let username = current_username();
        let host = CString::new(remote_addr).unwrap_or_default();
        let ip: IpAddr = remote_addr.parse().unwrap_or(IpAddr::from([0u8; 4]));

        let entry = Utmpx::new(
            UtType::USER_PROCESS,
            pid,
            &ut_line,
            &ut_id,
            &username,
            &host,
            Default::default(),
            0,
            now_tv(),
            ip,
        );

        match entry {
            Ok(e) => {
                if utmpx::write_line(&e).is_err() {
                    eprintln!("[etrs] utmp write failed");
                }
                write_wtmp(&e);
            }
            Err(_) => eprintln!("[etrs] utmp entry creation failed"),
        }
    }

    /// Write a DEAD_PROCESS utmp entry and append to wtmp.
    pub fn record_logout(pid: i32, tty_path: &Path) {
        let Some((ut_line, ut_id)) = tty_ids(tty_path) else {
            return;
        };

        let entry = Utmpx::new(
            UtType::DEAD_PROCESS,
            pid,
            &ut_line,
            &ut_id,
            &CString::new("").unwrap(),
            &CString::new("").unwrap(),
            Default::default(),
            0,
            now_tv(),
            IpAddr::from([0u8; 4]),
        );

        match entry {
            Ok(e) => {
                if utmpx::write_line(&e).is_err() {
                    eprintln!("[etrs] utmp logout write failed");
                }
                write_wtmp(&e);
            }
            Err(_) => eprintln!("[etrs] utmp logout entry creation failed"),
        }
    }

    fn now_tv() -> libc::timeval {
        let mut tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        unsafe { libc::gettimeofday(&mut tv, std::ptr::null_mut()) };
        tv
    }

    /// Derive `ut_line` (e.g. `pts/3`) and `ut_id` (e.g. `3`) from a PTY path.
    fn tty_ids(tty_path: &Path) -> Option<(CString, CString)> {
        let name = tty_path.to_str()?;
        let ut_line = name.strip_prefix("/dev/").unwrap_or(name);
        let ut_id = ut_line.rsplit('/').next().unwrap_or(ut_line);
        let ut_id = &ut_id[..ut_id.len().min(4)];
        Some((CString::new(ut_line).ok()?, CString::new(ut_id).ok()?))
    }

    fn current_username() -> CString {
        std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .ok()
            .and_then(|u| CString::new(u).ok())
            .unwrap_or_else(|| {
                let uid = unsafe { libc::getuid() };
                let pw = unsafe { libc::getpwuid(uid) };
                if !pw.is_null() {
                    unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) }.to_owned()
                } else {
                    CString::new("unknown").unwrap()
                }
            })
    }

    fn write_wtmp(entry: &Utmpx) {
        unsafe { updwtmpx(c"/var/log/wtmp".as_ptr(), entry as *const Utmpx) };
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::path::Path;

    pub fn record_login(_pid: i32, _tty_path: &Path, _remote_addr: &str) {}
    pub fn record_logout(_pid: i32, _tty_path: &Path) {}
}

pub use imp::{record_login, record_logout};
