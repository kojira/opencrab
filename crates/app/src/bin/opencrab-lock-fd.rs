//! Acquire a non-blocking exclusive lock on an inherited file descriptor.
//!
//! `dev.sh` opens the lock file before forking this helper.  `flock(2)` locks
//! the shared open-file description, so the lock remains held by the parent
//! shell after this short-lived helper exits and until that shell closes its
//! inherited descriptor.

use std::os::fd::RawFd;

fn main() {
    let fd: RawFd = std::env::args()
        .nth(1)
        .expect("usage: opencrab-lock-fd <fd>")
        .parse()
        .expect("fd must be an integer");

    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        eprintln!(
            "could not acquire launcher lock: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(1);
    }
}
