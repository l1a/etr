//! utmp/wtmp login record management via libutempter.
//!
//! libutempter forks a setgid-utmp helper (`/usr/libexec/utempter/utempter`) to
//! write login records without requiring `etrs` to hold elevated privileges.
//!
//! The library `dup2`s the master PTY fd to the helper's stdin/stdout; the helper
//! calls `ptsname(STDIN_FILENO)` to determine the slave pts name. Callers MUST
//! invoke [`record_login`] / [`record_logout`] from a dedicated thread
//! (`tokio::task::spawn_blocking`), not from an async context — the helper's
//! internal `fork+waitpid` is disrupted by tokio's `SIGCHLD` handling.
//!
//! Only compiled on Linux; other platforms get no-op stubs.

// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::CString;
    use std::os::unix::io::RawFd;

    unsafe extern "C" {
        // fd must be the master PTY; the helper calls ptsname(fd) internally.
        fn utempter_add_record(master_fd: RawFd, hostname: *const libc::c_char) -> libc::c_int;
        fn utempter_remove_added_record() -> libc::c_int;
    }

    /// Write a utmp/wtmp login entry for the session on `master_fd`.
    ///
    /// `remote_addr` is stored as the ut_host field (typically the client IP).
    pub fn record_login(master_fd: RawFd, remote_addr: &str) {
        let host = CString::new(remote_addr).unwrap_or_default();
        // SAFETY: utempter_add_record is an FFI call to libutempter; master_fd is
        // a valid open PTY fd passed in from the caller and outlives this call.
        unsafe { utempter_add_record(master_fd, host.as_ptr()) };
    }

    /// Remove the utmp/wtmp login entry previously written by [`record_login`].
    pub fn record_logout(_master_fd: RawFd) {
        // SAFETY: utempter_remove_added_record removes the record added by the
        // most recent utempter_add_record call in this process; no fd required.
        unsafe { utempter_remove_added_record() };
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::os::unix::io::RawFd;

    /// No-op on non-Linux platforms (utempter is Linux-only).
    pub fn record_login(_master_fd: RawFd, _remote_addr: &str) {}

    /// No-op on non-Linux platforms (utempter is Linux-only).
    pub fn record_logout(_master_fd: RawFd) {}
}

pub use imp::{record_login, record_logout};
