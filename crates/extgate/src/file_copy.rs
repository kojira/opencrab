//! Shared safe-copy primitives for core-owned attachment roots (inbound store
//! promotion and outbound outbox staging). The source is an already opened
//! no-follow descriptor; the destination is created exclusively at mode 0600,
//! streamed with a running sha256 and fsynced before success.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

/// Which side of a copy failed. Source failures are caller input problems;
/// destination failures are core storage problems.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyFailure {
    Source,
    Destination,
}

/// Size, digest and source identity of a completed copy.
pub(crate) struct CopiedFile {
    pub size: u64,
    pub sha256: String,
    pub identity: FileIdentity,
}

/// `(dev, ino)` of the copied source, used to detect replacement after copy.
pub(crate) struct FileIdentity {
    dev: u64,
    ino: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    pub fn matches(&self, metadata: &std::fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt as _;
        metadata.file_type().is_file() && self.dev == metadata.dev() && self.ino == metadata.ino()
    }
}

/// Open `path` read-only without following a symlink at the leaf.
pub(crate) fn open_leaf_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

/// Copy the regular file behind `input` to a new `destination` (create_new,
/// 0600, fsync). A partially written destination is removed on failure; an
/// existing destination is never touched.
pub(crate) fn copy_regular_file(
    input: &mut File,
    destination: &Path,
) -> Result<CopiedFile, CopyFailure> {
    let metadata = input.metadata().map_err(|_| CopyFailure::Source)?;
    if !metadata.is_file() {
        return Err(CopyFailure::Source);
    }
    let identity = FileIdentity::from_metadata(&metadata);
    let mut output = {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(destination)
            .map_err(|_| CopyFailure::Destination)?
    };
    match stream_and_sync(input, &mut output) {
        Ok((size, sha256)) => Ok(CopiedFile {
            size,
            sha256,
            identity,
        }),
        Err(failure) => {
            drop(output);
            let _ = std::fs::remove_file(destination);
            Err(failure)
        }
    }
}

fn stream_and_sync(input: &mut File, output: &mut File) -> Result<(u64, String), CopyFailure> {
    let mut hash = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer).map_err(|_| CopyFailure::Source)?;
        if read == 0 {
            break;
        }
        size = size.checked_add(read as u64).ok_or(CopyFailure::Source)?;
        hash.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .map_err(|_| CopyFailure::Destination)?;
    }
    output.sync_all().map_err(|_| CopyFailure::Destination)?;
    Ok((size, lower_hex(&hash.finalize())))
}

pub(crate) fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
