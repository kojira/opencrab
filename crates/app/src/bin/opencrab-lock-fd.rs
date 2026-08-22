//! Manage launcher locks and component PID publication on inherited file
//! descriptors.
//!
//! `dev.sh` opens the lock file before forking this helper.  `flock(2)` locks
//! the shared open-file description, so the lock remains held by the parent
//! shell after this short-lived helper exits and until that shell closes its
//! inherited descriptor.  The supervisor lock is non-blocking; lifecycle
//! transitions use `--wait` so they run one at a time.
//!
//! In `--publish-pid` mode this process atomically publishes its own PID while
//! it still holds the inherited lifecycle descriptor, closes that descriptor,
//! and then replaces itself with the component.  Thus lifecycle waiters cannot
//! run between component creation and PID registration.

use std::{
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::Write,
    os::{fd::RawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

const TEST_PAUSE_ENV: &str = "_OPENCRAB_DEV_TEST_PAUSE_BEFORE_PID_PUBLISH";

fn usage() -> ! {
    panic!(
        "usage: opencrab-lock-fd <fd> [--wait]\n       \
         opencrab-lock-fd <fd> --publish-pid <file> -- <command> [args...]"
    )
}

fn test_pause_before_publish(pid: u32) -> std::io::Result<()> {
    let Some(marker) = std::env::var_os(TEST_PAUSE_ENV) else {
        return Ok(());
    };
    let marker = PathBuf::from(marker);
    fs::write(&marker, format!("{pid}\n"))?;
    let mut release = marker.into_os_string();
    release.push(".release");
    while !Path::new(&release).exists() {
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn publish_pid_and_exec(fd: RawFd, pid_file: &OsStr, argv: Vec<OsString>) -> ! {
    if argv.is_empty() {
        usage();
    }

    // Fail before publication if the lifecycle descriptor was not inherited.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
        panic!(
            "launcher lifecycle descriptor {fd} is not open: {}",
            std::io::Error::last_os_error()
        );
    }

    let pid = std::process::id();
    let pid_file = PathBuf::from(pid_file);
    let mut temporary = pid_file.as_os_str().to_os_string();
    temporary.push(format!(".{pid}.tmp"));
    let temporary = PathBuf::from(temporary);

    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        writeln!(file, "{pid}")?;
        file.sync_all()?;

        test_pause_before_publish(pid)?;
        fs::rename(&temporary, &pid_file)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        panic!(
            "could not atomically publish component PID to {}: {error}",
            pid_file.display()
        );
    }

    // Publication must happen-before lifecycle unlock.  The component keeps
    // all other inherited descriptors, including its owner FD and FD 9.
    if unsafe { libc::close(fd) } != 0 {
        panic!(
            "could not close launcher lifecycle descriptor {fd}: {}",
            std::io::Error::last_os_error()
        );
    }

    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).env_remove(TEST_PAUSE_ENV);
    let error = command.exec();
    panic!("could not exec component {:?}: {error}", argv[0]);
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let fd: RawFd = args
        .next()
        .unwrap_or_else(|| usage())
        .into_string()
        .unwrap_or_else(|_| panic!("fd must be an integer"))
        .parse()
        .expect("fd must be an integer");
    let mode = args.next();
    if mode.as_deref() == Some(OsStr::new("--publish-pid")) {
        let pid_file = args.next().unwrap_or_else(|| usage());
        if args.next().as_deref() != Some(OsStr::new("--")) {
            usage();
        }
        publish_pid_and_exec(fd, &pid_file, args.collect());
    }

    let operation = match mode.as_deref() {
        None => libc::LOCK_EX | libc::LOCK_NB,
        Some(value) if value == OsStr::new("--wait") => libc::LOCK_EX,
        Some(_) => usage(),
    };
    assert!(
        args.next().is_none(),
        "unexpected extra launcher lock argument"
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
