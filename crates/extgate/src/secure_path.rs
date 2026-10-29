//! Directory-fd path resolution for opening outbound files without following symlinks.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

pub struct ParentDir {
    directory: File,
    name: CString,
}

fn component_name(component: &OsStr) -> io::Result<CString> {
    CString::new(component.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn openat_directory(parent_fd: i32, name: &CString) -> io::Result<File> {
    // SAFETY: `name` is NUL terminated; a nonnegative result is an owned descriptor.
    let fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: openat returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub fn open_parent(path: &Path) -> io::Result<ParentDir> {
    if !path.is_absolute() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let mut components = path.components().peekable();
    if components.next() != Some(Component::RootDir) {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    // SAFETY: static NUL-terminated root path; a nonnegative result is owned.
    let root_fd = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: open returned a new owned descriptor.
    let mut directory = unsafe { File::from_raw_fd(root_fd) };
    let mut final_name = None;
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        };
        let name = component_name(name)?;
        if components.peek().is_none() {
            final_name = Some(name);
        } else {
            directory = openat_directory(directory.as_raw_fd(), &name)?;
        }
    }
    Ok(ParentDir {
        directory,
        name: final_name.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?,
    })
}

/// Opens a regular file for reading without following a leaf symlink and
/// without blocking on a FIFO or device swapped in after an earlier check.
pub fn open_regular_file_no_follow_nonblocking(path: &Path) -> io::Result<File> {
    let parent = open_parent(path)?;
    // SAFETY: descriptor and name are valid; a nonnegative result is owned.
    let fd = unsafe {
        libc::openat(
            parent.directory.as_raw_fd(),
            parent.name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    // SAFETY: valid owned descriptor; F_GETFL/F_SETFL take no pointers.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        // SAFETY: as above.
        || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::time::Duration;

    fn open_with_timeout(path: std::path::PathBuf) -> io::Result<File> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(open_regular_file_no_follow_nonblocking(&path));
        });
        rx.recv_timeout(Duration::from_secs(3))
            .expect("open must not block on a FIFO")
    }

    #[test]
    fn open_regular_file_no_follow_nonblocking_rejects_fifo_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().canonicalize().unwrap().join("swap.fifo");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(open_with_timeout(fifo).is_err());
    }

    #[test]
    fn open_regular_file_no_follow_nonblocking_reads_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("ok.txt");
        std::fs::write(&path, b"hello").unwrap();
        let mut file = open_with_timeout(path).unwrap();
        // SAFETY: valid owned descriptor.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(flags & libc::O_NONBLOCK, 0, "O_NONBLOCK must be cleared");
        let mut body = String::new();
        file.read_to_string(&mut body).unwrap();
        assert_eq!(body, "hello");
    }
}
