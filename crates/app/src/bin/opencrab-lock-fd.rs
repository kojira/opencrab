//! Acquire an exclusive lock on an inherited file descriptor.
//!
//! `dev.sh` opens the lock file before forking this helper.  `flock(2)` locks
//! the shared open-file description, so the lock remains held by the parent
//! shell after this short-lived helper exits and until that shell closes its
//! inherited descriptor.  The supervisor lock is non-blocking; lifecycle
//! transitions use `--wait` so they run one at a time.

use std::os::fd::RawFd;

fn main() {
    let mut args = std::env::args().skip(1);
    let fd: RawFd = args
        .next()
        .expect("usage: opencrab-lock-fd <fd> [--wait]")
        .parse()
        .expect("fd must be an integer");
    let operation = match args.next().as_deref() {
        None => libc::LOCK_EX | libc::LOCK_NB,
        Some("--wait") => libc::LOCK_EX,
        Some(_) => panic!("usage: opencrab-lock-fd <fd> [--wait]"),
    };
    assert!(
        args.next().is_none(),
        "usage: opencrab-lock-fd <fd> [--wait]"
    );

    let rc = unsafe { libc::flock(fd, operation) };
    if rc != 0 {
        eprintln!(
            "could not acquire launcher lock: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(1);
    }
}
