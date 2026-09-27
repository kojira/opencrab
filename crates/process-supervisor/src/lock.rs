//! Exclusive daemon ownership lock for one durable store.

use std::fs::{File, OpenOptions};
use std::path::Path;

pub struct StoreLock {
    _file: File,
}

impl StoreLock {
    pub fn acquire(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            // SAFETY: the fd belongs to `file` and remains open for the guard lifetime.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn s5_store_lock_refuses_second_daemon() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("owner.lock");
        let _first = StoreLock::acquire(&path).unwrap();
        assert!(StoreLock::acquire(&path).is_err());
    }
}
