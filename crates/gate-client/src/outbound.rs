//! Outbound file descriptors produced by core for declared
//! `"format": "workspace-file"` operation fields.
//!
//! Core copies the agent's workspace file into its outbox and replaces the
//! field value with `{id, name, media_type, size, sha256, local_path}`, where
//! `local_path` is relative to the outbox root shared with the gateway. A
//! gateway parses each descriptor with [`OutboundFile::parse`] and reads the
//! bytes with [`open_verified`], which refuses anything that is not exactly the
//! copy core described. Raw strings (an older core that does not rewrite) and
//! other non-object values are errors, so the gateway fails closed.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// One verified-shape outbound file descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundFile {
    pub id: String,
    pub name: String,
    pub media_type: Option<String>,
    pub size: u64,
    pub sha256: String,
    pub local_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundFileError {
    /// The value is not a well-formed descriptor object.
    Malformed,
    /// The outbox copy is missing, not a regular file, or behind a symlink.
    Unreadable,
    /// The outbox copy's size or sha256 differs from the descriptor.
    Mismatch,
}

impl std::fmt::Display for OutboundFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Malformed => "outbound file descriptor is malformed",
            Self::Unreadable => "outbound file copy is unreadable",
            Self::Mismatch => "outbound file copy does not match its descriptor",
        })
    }
}

impl std::error::Error for OutboundFileError {}

impl OutboundFile {
    /// Parse one descriptor object. Any other JSON value is `Malformed`.
    pub fn parse(value: &Value) -> Result<Self, OutboundFileError> {
        let obj = value.as_object().ok_or(OutboundFileError::Malformed)?;
        let text = |key: &str| match obj.get(key) {
            Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
            _ => Err(OutboundFileError::Malformed),
        };
        let id = text("id")?;
        let name = text("name")?;
        if name.chars().count() > 255
            || name
                .chars()
                .any(|c| c.is_control() || c == '/' || c == '\\')
        {
            return Err(OutboundFileError::Malformed);
        }
        let media_type = match obj.get("media_type") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if !s.is_empty() && s.is_ascii() => Some(s.clone()),
            _ => return Err(OutboundFileError::Malformed),
        };
        let size = obj
            .get("size")
            .and_then(Value::as_u64)
            .ok_or(OutboundFileError::Malformed)?;
        let sha256 = text("sha256")?;
        if sha256.len() != 64
            || !sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(OutboundFileError::Malformed);
        }
        let local_path = text("local_path")?;
        if !is_normal_relative(&local_path) {
            return Err(OutboundFileError::Malformed);
        }
        Ok(Self {
            id,
            name,
            media_type,
            size,
            sha256,
            local_path,
        })
    }

    /// Parse a declared field value: one descriptor object or an array of them.
    pub fn parse_list(value: &Value) -> Result<Vec<Self>, OutboundFileError> {
        match value {
            Value::Array(items) => items.iter().map(Self::parse).collect(),
            other => Self::parse(other).map(|file| vec![file]),
        }
    }
}

fn is_normal_relative(path: &str) -> bool {
    let path = Path::new(path);
    !path.is_absolute()
        && path.components().next().is_some()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// Read the outbox copy described by `file` under `outbox_root` and return its
/// bytes after re-verifying size and sha256. Symlinked directories or leaves
/// are refused; the leaf is opened with `O_NOFOLLOW`.
pub fn open_verified(
    outbox_root: &Path,
    file: &OutboundFile,
) -> Result<Vec<u8>, OutboundFileError> {
    if !is_normal_relative(&file.local_path) {
        return Err(OutboundFileError::Malformed);
    }
    let root = outbox_root
        .canonicalize()
        .map_err(|_| OutboundFileError::Unreadable)?;
    let lexical: PathBuf = root.join(&file.local_path);
    let canonical = lexical
        .canonicalize()
        .map_err(|_| OutboundFileError::Unreadable)?;
    if canonical != lexical {
        return Err(OutboundFileError::Unreadable);
    }
    let mut input = {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lexical)
            .map_err(|_| OutboundFileError::Unreadable)?
    };
    let metadata = input
        .metadata()
        .map_err(|_| OutboundFileError::Unreadable)?;
    if !metadata.is_file() {
        return Err(OutboundFileError::Unreadable);
    }
    if metadata.len() != file.size {
        return Err(OutboundFileError::Mismatch);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(file.size).unwrap_or(0));
    input
        .by_ref()
        .take(file.size.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| OutboundFileError::Unreadable)?;
    if bytes.len() as u64 != file.size || lower_hex(&Sha256::digest(&bytes)) != file.sha256 {
        return Err(OutboundFileError::Mismatch);
    }
    Ok(bytes)
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(bytes: &[u8]) -> (tempfile::TempDir, OutboundFile) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("instance-1")).unwrap();
        std::fs::write(dir.path().join("instance-1/copy.bin"), bytes).unwrap();
        let file = OutboundFile::parse(&json!({
            "id": "0b9c1f6e-6a43-4c55-9d43-2f7f0b0c8a11",
            "name": "x.png",
            "media_type": "image/png",
            "size": bytes.len(),
            "sha256": lower_hex(&Sha256::digest(bytes)),
            "local_path": "instance-1/copy.bin",
        }))
        .unwrap();
        (dir, file)
    }

    #[test]
    fn open_verified_returns_bytes_of_matching_copy() {
        let (dir, file) = fixture(b"image-bytes");
        assert_eq!(open_verified(dir.path(), &file).unwrap(), b"image-bytes");
        assert_eq!(file.media_type.as_deref(), Some("image/png"));
    }

    #[test]
    fn open_verified_rejects_sha_mismatch() {
        let (dir, mut file) = fixture(b"image-bytes");
        file.sha256 = "0".repeat(64);
        assert_eq!(
            open_verified(dir.path(), &file),
            Err(OutboundFileError::Mismatch)
        );
    }

    #[test]
    fn open_verified_rejects_size_mismatch() {
        let (dir, mut file) = fixture(b"image-bytes");
        file.size += 1;
        assert_eq!(
            open_verified(dir.path(), &file),
            Err(OutboundFileError::Mismatch)
        );
        std::fs::write(dir.path().join("instance-1/copy.bin"), b"image-bytes!").unwrap();
        file.size -= 1;
        assert_eq!(
            open_verified(dir.path(), &file),
            Err(OutboundFileError::Mismatch)
        );
    }

    #[test]
    fn open_verified_rejects_symlinked_leaf_and_directory() {
        let (dir, file) = fixture(b"image-bytes");
        let leaf = OutboundFile {
            local_path: "instance-1/link.bin".into(),
            ..file.clone()
        };
        std::os::unix::fs::symlink(
            dir.path().join("instance-1/copy.bin"),
            dir.path().join("instance-1/link.bin"),
        )
        .unwrap();
        assert_eq!(
            open_verified(dir.path(), &leaf),
            Err(OutboundFileError::Unreadable)
        );
        std::os::unix::fs::symlink(dir.path().join("instance-1"), dir.path().join("alias"))
            .unwrap();
        let via_dir = OutboundFile {
            local_path: "alias/copy.bin".into(),
            ..file
        };
        assert_eq!(
            open_verified(dir.path(), &via_dir),
            Err(OutboundFileError::Unreadable)
        );
    }

    #[test]
    fn traversal_and_absolute_local_paths_are_malformed() {
        let (dir, file) = fixture(b"image-bytes");
        for bad in [
            "../copy.bin",
            "instance-1/../instance-1/copy.bin",
            "/etc/passwd",
            "./x",
        ] {
            let mut value = json!({
                "id": file.id, "name": file.name, "media_type": null, "size": file.size,
                "sha256": file.sha256, "local_path": bad,
            });
            assert_eq!(
                OutboundFile::parse(&value),
                Err(OutboundFileError::Malformed),
                "{bad}"
            );
            value["local_path"] = json!("instance-1/copy.bin");
            assert!(OutboundFile::parse(&value).is_ok());
            let direct = OutboundFile {
                local_path: bad.into(),
                ..file.clone()
            };
            assert_eq!(
                open_verified(dir.path(), &direct),
                Err(OutboundFileError::Malformed)
            );
        }
    }

    #[test]
    fn non_object_and_incomplete_values_are_malformed() {
        for value in [
            json!("generated_images/x.png"),
            json!(null),
            json!(1),
            json!({"name": "x"}),
            json!(["raw/path.png"]),
        ] {
            assert_eq!(
                OutboundFile::parse_list(&value),
                Err(OutboundFileError::Malformed),
                "{value}"
            );
        }
    }
}
