//! Directory-fd preparation and replacement-safe cleanup for the core gate-admin UDS.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use crate::gate_admin_security::SecurityError;
use crate::secure_path::ParentDir;

#[derive(Clone, Copy)]
struct SocketIdentity {
    device: libc::dev_t,
    inode: libc::ino_t,
    uid: libc::uid_t,
}

pub struct PreparedAdminSocket {
    listener: Option<tokio::net::UnixListener>,
    cleanup: Option<SocketCleanup>,
}

pub struct SocketCleanup {
    parent: ParentDir,
    identity: SocketIdentity,
}

impl PreparedAdminSocket {
    pub fn into_parts(mut self) -> (tokio::net::UnixListener, SocketCleanup) {
        let listener = self.listener.take().expect("prepared listener");
        let cleanup = self.cleanup.take().expect("prepared cleanup identity");
        (listener, cleanup)
    }
}

impl Drop for PreparedAdminSocket {
    fn drop(&mut self) {
        if let Some(cleanup) = &self.cleanup {
            cleanup.remove_if_same_inode();
        }
    }
}

fn matches_identity(stat: &libc::stat, identity: SocketIdentity) -> bool {
    (stat.st_mode & libc::S_IFMT) == libc::S_IFSOCK
        && stat.st_dev == identity.device
        && stat.st_ino == identity.inode
        && stat.st_uid == identity.uid
}

impl SocketCleanup {
    pub fn remove_if_same_inode(&self) {
        let quarantine =
            match CString::new(format!(".opencrab-admin-cleanup-{}", uuid::Uuid::new_v4())) {
                Ok(name) => name,
                Err(_) => return,
            };
        if self.parent.rename_child(&quarantine).is_err() {
            return;
        }
        match self.parent.stat_named(&quarantine) {
            Ok(stat) if matches_identity(&stat, self.identity) => {
                let _ = self.parent.unlink_named(&quarantine);
            }
            Ok(_) | Err(_) => {
                // A replacement is never unlinked. Restore it only if the original name is free.
                if self.parent.child_stat().is_err() {
                    // SAFETY: descriptors and NUL-terminated names remain valid for the call.
                    let _ = unsafe {
                        libc::renameat(
                            self.parent.directory().as_raw_fd(),
                            quarantine.as_ptr(),
                            self.parent.directory().as_raw_fd(),
                            self.parent.name().as_ptr(),
                        )
                    };
                }
            }
        }
    }
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        self.remove_if_same_inode();
    }
}

fn send_fd(channel: RawFd, fd: RawFd) -> io::Result<()> {
    let mut byte = 1_u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control = [0_usize; 8];
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = (control.len() * mem::size_of::<usize>()) as _;
    // SAFETY: message points to initialized iovec/control storage of the advertised sizes.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() {
            return Err(io::Error::other("SCM_RIGHTS header unavailable"));
        }
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
        message.msg_controllen = (*header).cmsg_len;
        if libc::sendmsg(channel, &message, 0) != 1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn receive_fd(channel: RawFd) -> io::Result<RawFd> {
    let mut byte = 0_u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control = [0_usize; 8];
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = (control.len() * mem::size_of::<usize>()) as _;
    // SAFETY: message points to writable iovec/control storage of the advertised sizes.
    let received = unsafe { libc::recvmsg(channel, &mut message, 0) };
    if received != 1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: recvmsg initialized headers within the supplied control buffer.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::other("SCM_RIGHTS descriptor missing"));
        }
        Ok(std::ptr::read(libc::CMSG_DATA(header).cast::<RawFd>()))
    }
}

fn child_bind(parent: &File, name: &CString, channel: RawFd) -> ! {
    // The post-fork child invokes only libc syscalls and exits without touching shared Rust state.
    let result = unsafe {
        libc::umask(0o177);
        if libc::fchdir(parent.as_raw_fd()) != 0 {
            -1
        } else {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            if fd < 0 {
                -1
            } else {
                let mut address: libc::sockaddr_un = mem::zeroed();
                address.sun_family = libc::AF_UNIX as libc::sa_family_t;
                let bytes = name.as_bytes_with_nul();
                if bytes.len() > address.sun_path.len() {
                    libc::close(fd);
                    -1
                } else {
                    std::ptr::copy_nonoverlapping(
                        bytes.as_ptr().cast::<libc::c_char>(),
                        address.sun_path.as_mut_ptr(),
                        bytes.len(),
                    );
                    let length = mem::size_of::<libc::sa_family_t>() + bytes.len();
                    #[cfg(any(target_os = "macos", target_os = "ios"))]
                    {
                        address.sun_len = length as u8;
                    }
                    let bound = libc::bind(
                        fd,
                        (&raw const address).cast::<libc::sockaddr>(),
                        length as libc::socklen_t,
                    ) == 0;
                    let listened = bound && libc::listen(fd, 128) == 0;
                    let sent = listened && send_fd(channel, fd).is_ok();
                    libc::close(fd);
                    i32::from(sent)
                }
            }
        }
    };
    // SAFETY: immediate process termination avoids non-async-signal-safe destructors after fork.
    unsafe { libc::_exit(if result == 1 { 0 } else { 1 }) }
}

fn bind_relative_to(parent: &ParentDir) -> io::Result<std::os::unix::net::UnixListener> {
    let mut channels = [0; 2];
    // SAFETY: channels points to two writable descriptors.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, channels.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fork is followed in the child only by the syscall-only `child_bind` path.
    let child = unsafe { libc::fork() };
    if child < 0 {
        unsafe {
            libc::close(channels[0]);
            libc::close(channels[1]);
        }
        return Err(io::Error::last_os_error());
    }
    if child == 0 {
        unsafe { libc::close(channels[0]) };
        child_bind(parent.directory(), parent.name(), channels[1]);
    }
    unsafe { libc::close(channels[1]) };
    let received = receive_fd(channels[0]);
    unsafe { libc::close(channels[0]) };
    let mut status = 0;
    // SAFETY: `child` is the direct child created above and status is writable.
    let waited = unsafe { libc::waitpid(child, &mut status, 0) };
    let fd = received?;
    if waited != child || !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        unsafe { libc::close(fd) };
        return Err(io::Error::other("directory-fd socket bind failed"));
    }
    // SAFETY: SCM_RIGHTS yielded a new owned listener descriptor.
    let listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) };
    listener.set_nonblocking(true)?;
    Ok(listener)
}

pub fn prepare_admin_socket(
    path: &Path,
    service_euid: u32,
) -> Result<PreparedAdminSocket, SecurityError> {
    let parent = crate::secure_path::open_parent(path).map_err(|_| SecurityError::InvalidConfig)?;
    let parent_metadata = parent
        .metadata()
        .map_err(|_| SecurityError::InvalidConfig)?;
    if parent_metadata.uid() != service_euid || parent_metadata.permissions().mode() & 0o022 != 0 {
        return Err(SecurityError::InvalidConfig);
    }
    match parent.child_stat() {
        Ok(_) => return Err(SecurityError::Conflict),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(SecurityError::InvalidConfig),
    }
    let listener = bind_relative_to(&parent).map_err(|_| SecurityError::Store)?;
    let created = parent.child_stat().map_err(|_| SecurityError::Store)?;
    let identity = SocketIdentity {
        device: created.st_dev,
        inode: created.st_ino,
        uid: created.st_uid,
    };
    if !matches_identity(&created, identity)
        || created.st_uid != service_euid
        || created.st_mode & 0o777 != 0o600
    {
        return Err(SecurityError::Store);
    }
    let listener =
        tokio::net::UnixListener::from_std(listener).map_err(|_| SecurityError::Store)?;
    let cleanup = SocketCleanup {
        parent: parent.try_clone().map_err(|_| SecurityError::Store)?,
        identity,
    };
    Ok(PreparedAdminSocket {
        listener: Some(listener),
        cleanup: Some(cleanup),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_and_socket_paths_use_directory_fd_no_follow_operations() {
        let manifest_source = include_str!("gate_admin_security.rs");
        let socket_source = include_str!("admin_socket.rs");
        let path_source = include_str!("secure_path.rs");
        assert!(
            path_source.contains("openat("),
            "manifest opening must use a directory-fd component walk"
        );
        assert!(
            socket_source.contains("openat(") || path_source.contains("openat("),
            "socket preparation must use a directory-fd component walk"
        );
        assert!(
            path_source.contains("unlinkat("),
            "socket cleanup must stay relative to a held parent directory fd"
        );
        assert!(!manifest_source.contains("symlink_metadata(&current)"));
    }

    #[tokio::test]
    async fn socket_is_private_refuses_stale_path_and_cleanup_is_inode_safe() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = temp.path().canonicalize().unwrap().join("admin.sock");
        let prepared = prepare_admin_socket(&path, unsafe { libc::geteuid() }).unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert!(matches!(
            prepare_admin_socket(&path, unsafe { libc::geteuid() }),
            Err(SecurityError::Conflict)
        ));
        let (_listener, cleanup) = prepared.into_parts();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        cleanup.remove_if_same_inode();
        assert!(
            path.is_file(),
            "cleanup must not unlink a replacement inode"
        );
    }

    #[tokio::test]
    async fn socket_parent_fd_survives_component_replacement_and_cleanup_preserves_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let original = root.join("original");
        std::fs::create_dir(&original).unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = original.join("admin.sock");
        let prepared = prepare_admin_socket(&path, unsafe { libc::geteuid() }).unwrap();
        let (_listener, cleanup) = prepared.into_parts();
        let moved = root.join("moved");
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        cleanup.remove_if_same_inode();
        assert!(path.is_file());
        assert!(!moved.join("admin.sock").exists());
    }

    #[tokio::test]
    async fn socket_rejects_insecure_or_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            prepare_admin_socket(&root.join("insecure.sock"), unsafe { libc::geteuid() }),
            Err(SecurityError::InvalidConfig)
        ));

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let actual = root.join("actual");
        std::fs::create_dir(&actual).unwrap();
        std::fs::set_permissions(&actual, std::fs::Permissions::from_mode(0o700)).unwrap();
        let linked = root.join("linked");
        symlink(&actual, &linked).unwrap();
        assert!(matches!(
            prepare_admin_socket(&linked.join("symlink.sock"), unsafe { libc::geteuid() }),
            Err(SecurityError::InvalidConfig)
        ));
    }
}
