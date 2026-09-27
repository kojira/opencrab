//! Directory-fd path resolution for security-sensitive startup files.

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

impl ParentDir {
    pub fn directory(&self) -> &File {
        &self.directory
    }

    pub fn name(&self) -> &CString {
        &self.name
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            directory: self.directory.try_clone()?,
            name: self.name.clone(),
        })
    }

    pub fn metadata(&self) -> io::Result<std::fs::Metadata> {
        self.directory.metadata()
    }

    pub fn child_stat(&self) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: both pointers are valid and fstatat initializes `stat` on success.
        let result = unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                self.name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == 0 {
            // SAFETY: successful fstatat initialized every field.
            Ok(unsafe { stat.assume_init() })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn rename_child(&self, destination: &CString) -> io::Result<()> {
        // SAFETY: directory descriptors and both NUL-terminated names remain valid for the call.
        let result = unsafe {
            libc::renameat(
                self.directory.as_raw_fd(),
                self.name.as_ptr(),
                self.directory.as_raw_fd(),
                destination.as_ptr(),
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn stat_named(&self, name: &CString) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: both pointers are valid and fstatat initializes `stat` on success.
        let result = unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == 0 {
            // SAFETY: successful fstatat initialized every field.
            Ok(unsafe { stat.assume_init() })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn unlink_named(&self, name: &CString) -> io::Result<()> {
        // SAFETY: the descriptor and NUL-terminated child name are valid for unlinkat.
        let result = unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
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

pub fn open_file_no_follow(path: &Path) -> io::Result<File> {
    let parent = open_parent(path)?;
    // SAFETY: descriptor and name are valid; a nonnegative result is owned.
    let fd = unsafe {
        libc::openat(
            parent.directory.as_raw_fd(),
            parent.name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: openat returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
