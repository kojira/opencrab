//! Secure preparation and inode-safe cleanup for the core gate-admin UDS.

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use crate::gate_admin_security::SecurityError;

#[derive(Clone, Copy)]
struct SocketIdentity {
    device: u64,
    inode: u64,
    uid: u32,
}

pub struct PreparedAdminSocket {
    listener: Option<tokio::net::UnixListener>,
    cleanup: Option<SocketCleanup>,
}

pub struct SocketCleanup {
    path: PathBuf,
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

impl SocketCleanup {
    pub fn remove_if_same_inode(&self) {
        let Ok(metadata) = std::fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket()
            && metadata.dev() == self.identity.device
            && metadata.ino() == self.identity.inode
            && metadata.uid() == self.identity.uid
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        self.remove_if_same_inode();
    }
}

pub fn prepare_admin_socket(
    path: &Path,
    service_euid: u32,
) -> Result<PreparedAdminSocket, SecurityError> {
    if !path.is_absolute() {
        return Err(SecurityError::InvalidConfig);
    }
    let parent = path.parent().ok_or(SecurityError::InvalidConfig)?;
    let mut current = PathBuf::from("/");
    for component in parent.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(part) => current.push(part),
            _ => return Err(SecurityError::InvalidConfig),
        }
        let metadata =
            std::fs::symlink_metadata(&current).map_err(|_| SecurityError::InvalidConfig)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(SecurityError::InvalidConfig);
        }
    }
    let parent_metadata = std::fs::metadata(parent).map_err(|_| SecurityError::InvalidConfig)?;
    if parent_metadata.uid() != service_euid || parent_metadata.permissions().mode() & 0o022 != 0 {
        return Err(SecurityError::InvalidConfig);
    }
    match std::fs::symlink_metadata(path) {
        Ok(_) => return Err(SecurityError::Conflict),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(SecurityError::InvalidConfig),
    }
    let listener = tokio::net::UnixListener::bind(path).map_err(|_| SecurityError::Store)?;
    let created = std::fs::symlink_metadata(path).map_err(|_| SecurityError::Store)?;
    if !created.file_type().is_socket() || created.uid() != service_euid {
        return Err(SecurityError::Store);
    }
    let identity = SocketIdentity {
        device: created.dev(),
        inode: created.ino(),
        uid: created.uid(),
    };
    let cleanup = SocketCleanup {
        path: path.to_owned(),
        identity,
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| SecurityError::Store)?;
    let metadata = std::fs::symlink_metadata(path).map_err(|_| SecurityError::Store)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != service_euid
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.dev() != identity.device
        || metadata.ino() != identity.inode
    {
        return Err(SecurityError::Store);
    }
    Ok(PreparedAdminSocket {
        listener: Some(listener),
        cleanup: Some(cleanup),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
