// SPDX-License-Identifier: GPL-3.0-or-later
//
// utmp/wtmp login record management via libutempter.
//
// libutempter forks a setgid-utmp helper (/usr/libexec/utempter/utempter) to
// write login records without requiring etrs to hold elevated privileges.
//
// The library dup2's the master PTY fd to the helper's stdin/stdout; the helper
// calls ptsname(STDIN_FILENO) to determine the slave pts name.  Callers MUST
// invoke record_login / record_logout from a dedicated thread (tokio::spawn_blocking),
// not from an async context — the helper's internal fork+waitpid is disrupted by
// tokio's SIGCHLD handling.
//
// Only compiled on Linux — other platforms get no-op stubs.

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::CString;
    use std::os::unix::io::RawFd;

    unsafe extern "C" {
        // fd must be the master PTY; the helper calls ptsname(fd) internally.
        fn utempter_add_record(master_fd: RawFd, hostname: *const libc::c_char) -> libc::c_int;
        fn utempter_remove_added_record() -> libc::c_int;
    }

    pub fn record_login(master_fd: RawFd, remote_addr: &str) {
        let host = CString::new(remote_addr).unwrap_or_default();
        unsafe { utempter_add_record(master_fd, host.as_ptr()) };
    }

    pub fn record_logout(_master_fd: RawFd) {
        unsafe { utempter_remove_added_record() };
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::os::unix::io::RawFd;

    pub fn record_login(_master_fd: RawFd, _remote_addr: &str) {}
    pub fn record_logout(_master_fd: RawFd) {}
}

pub use imp::{record_login, record_logout};
