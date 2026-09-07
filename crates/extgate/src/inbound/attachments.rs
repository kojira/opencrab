use std::path::{Path, PathBuf};

use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::error::{ErrorCode, GateError};
use crate::protocol::{Said, SaidAttachment};
use crate::registry::ExtgateState;

/// Validate gateway-local files and create provider-neutral prompt/image parts.
/// Originals remain in the core-owned attachment root and are never represented
/// by absolute paths in the conversation or database.
pub(super) fn materialize(state: &ExtgateState, said: &Said) -> Result<Said, GateError> {
    if !said
        .attachments
        .iter()
        .any(|item| matches!(item, SaidAttachment::LocalFile { .. }))
    {
        return Ok(said.clone());
    }
    let root = state
        .attachment_inbox_root()
        .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
    let root = root
        .canonicalize()
        .map_err(|_| GateError::new(ErrorCode::BadRequest))?;
    let mut out = said.clone();
    let mut order = Vec::new();
    let mut text_parts = Vec::new();
    let mut image_parts = Vec::new();
    for (index, attachment) in said.attachments.iter().enumerate() {
        let SaidAttachment::LocalFile {
            id,
            name,
            media_type,
            size,
            sha256,
            local_path,
        } = attachment
        else {
            continue;
        };
        let path = validate_file(&root, local_path, *size, sha256)?;
        let bytes = std::fs::read(path).map_err(|_| GateError::new(ErrorCode::BadRequest))?;
        let mime = media_type.as_deref().unwrap_or("application/octet-stream");
        order.push(format!("{}. {} ({}, id {})", index + 1, name, mime, id));
        if is_text(mime) {
            let content =
                std::str::from_utf8(&bytes).map_err(|_| GateError::new(ErrorCode::BadRequest))?;
            text_parts.push(format!(
                "<attachment index=\"{}\" id=\"{}\" name=\"{}\" media_type=\"{}\">\n{}\n</attachment>",
                index + 1,
                id,
                escape_attr(name),
                escape_attr(mime),
                content
            ));
        } else if is_verified_image(mime, &bytes) {
            image_parts.push(SaidAttachment::ImageUrl(format!(
                "data:{mime};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            )));
        } else {
            text_parts.push(format!(
                "[Attachment: {} ({}, {} bytes, id {}) — content extraction unsupported]",
                name, mime, size, id
            ));
        }
    }
    if !order.is_empty() || !text_parts.is_empty() {
        if !out.text.is_empty() {
            out.text.push_str("\n\n");
        }
        if !order.is_empty() {
            out.text.push_str("<attachment-order>\n");
            out.text.push_str(&order.join("\n"));
            out.text.push_str("\n</attachment-order>");
        }
        if !text_parts.is_empty() {
            out.text.push_str("\n\n");
            out.text.push_str(&text_parts.join("\n\n"));
        }
    }
    out.attachments.extend(image_parts);
    Ok(out)
}

/// Move admitted originals from the gateway inbox to a content-addressed store.
/// `hard_link` is no-replace: a gateway-provided UUID can never overwrite an
/// earlier attachment, and the linked inode is revalidated before source removal.
pub(super) fn promote_local_files(
    state: &ExtgateState,
    said: &Said,
) -> Result<Vec<PathBuf>, GateError> {
    if !said
        .attachments
        .iter()
        .any(|item| matches!(item, SaidAttachment::LocalFile { .. }))
    {
        return Ok(Vec::new());
    }
    let inbox = state
        .attachment_inbox_root()
        .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
    let base = inbox
        .parent()
        .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
    let store = base.join("store");
    std::fs::create_dir_all(&store).map_err(|_| GateError::store())?;
    let store = store.canonicalize().map_err(|_| GateError::store())?;
    let mut created = Vec::new();
    let mut sources = Vec::new();
    for attachment in &said.attachments {
        if let SaidAttachment::LocalFile {
            local_path,
            size,
            sha256,
            ..
        } = attachment
        {
            let source = validate_file(&inbox, local_path, *size, sha256)?;
            let file_name = format!("{sha256}.bin");
            let destination = store.join(&file_name);
            match std::fs::hard_link(&source, &destination) {
                Ok(()) => created.push(destination),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    validate_file(&store, &file_name, *size, sha256)?;
                }
                Err(_) => {
                    remove_promoted_files(&created);
                    return Err(GateError::store());
                }
            }
            sources.push(source);
        }
    }
    for source in sources {
        if std::fs::remove_file(source).is_err() {
            remove_promoted_files(&created);
            return Err(GateError::store());
        }
    }
    Ok(created)
}

pub(super) fn remove_promoted_files(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

pub(super) fn remove_local_files(state: &ExtgateState, said: &Said) {
    let Some(root) = state.attachment_inbox_root() else {
        return;
    };
    for attachment in &said.attachments {
        if let SaidAttachment::LocalFile { local_path, .. } = attachment {
            let path = root.join(local_path);
            if path
                .symlink_metadata()
                .is_ok_and(|meta| meta.file_type().is_file())
            {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn validate_file(
    root: &Path,
    relative: &str,
    expected_size: u64,
    expected_hash: &str,
) -> Result<PathBuf, GateError> {
    let joined = root.join(relative);
    let link_meta = joined
        .symlink_metadata()
        .map_err(|_| GateError::new(ErrorCode::BadRequest))?;
    if !link_meta.file_type().is_file() || link_meta.len() != expected_size {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    let canonical = joined
        .canonicalize()
        .map_err(|_| GateError::new(ErrorCode::BadRequest))?;
    if !canonical.starts_with(root) {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    let bytes = std::fs::read(&canonical).map_err(|_| GateError::new(ErrorCode::BadRequest))?;
    if bytes.len() as u64 != expected_size || lower_hex(&Sha256::digest(&bytes)) != expected_hash {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    Ok(canonical)
}

fn is_text(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime.split(';').next().unwrap_or(mime),
            "application/json" | "application/xml" | "application/javascript"
        )
}

fn is_verified_image(mime: &str, bytes: &[u8]) -> bool {
    match mime.split(';').next().unwrap_or(mime) {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_image_classification_is_conservative() {
        assert!(is_text("text/html; charset=utf-8"));
        assert!(is_text("application/json"));
        assert!(is_verified_image("image/png", b"\x89PNG\r\n\x1a\nrest"));
        assert!(!is_verified_image("image/png", b"not a png"));
    }

    #[test]
    fn validates_exact_size_and_hash_under_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance/origin/file.bin");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"hello").unwrap();
        let hash = lower_hex(&Sha256::digest(b"hello"));
        let root = dir.path().canonicalize().unwrap();
        assert_eq!(
            validate_file(&root, "instance/origin/file.bin", 5, &hash).unwrap(),
            path.canonicalize().unwrap()
        );
        assert!(validate_file(&root, "instance/origin/file.bin", 4, &hash).is_err());
        assert!(validate_file(&root, "instance/origin/file.bin", 5, &"0".repeat(64)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_even_when_target_is_under_root() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"hello").unwrap();
        symlink(&target, dir.path().join("link")).unwrap();
        let hash = lower_hex(&Sha256::digest(b"hello"));
        let root = dir.path().canonicalize().unwrap();
        assert!(validate_file(&root, "link", 5, &hash).is_err());
    }
}
