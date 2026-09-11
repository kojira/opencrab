use std::path::{Path, PathBuf};

use base64::Engine as _;

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
            local_path,
            ..
        } = attachment
        else {
            continue;
        };
        let path = validate_local_path(&root, local_path)?;
        let bytes = std::fs::read(path).map_err(|_| GateError::new(ErrorCode::BadRequest))?;
        let actual_size = bytes.len() as u64;
        let detected_image = image_media_type(&bytes);
        let mime = detected_image
            .or(media_type.as_deref())
            .unwrap_or("application/octet-stream");
        if let SaidAttachment::LocalFile {
            media_type, size, ..
        } = &mut out.attachments[index]
        {
            *media_type = Some(mime.to_string());
            *size = actual_size;
        }
        if let Some(image_mime) = detected_image {
            order.push(format!(
                "{}. {} — [画像添付: {} ({})]",
                index + 1,
                escape_attr(name),
                escape_attr(name),
                image_mime
            ));
            image_parts.push(SaidAttachment::ImageUrl(format!(
                "data:{image_mime};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            )));
        } else if is_text(mime) {
            order.push(format!("{}. {} ({}, id {})", index + 1, name, mime, id));
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
        } else {
            order.push(format!("{}. {} ({}, id {})", index + 1, name, mime, id));
            text_parts.push(format!(
                "[Attachment: {} ({}, {} bytes, id {}) — content extraction unsupported]",
                name, mime, actual_size, id
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

/// Move admitted originals from the private inbox to UUID-named durable storage.
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
    #[cfg(unix)]
    std::fs::set_permissions(&store, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .map_err(|_| GateError::store())?;
    let store = store.canonicalize().map_err(|_| GateError::store())?;
    let mut created = Vec::new();
    let mut sources = Vec::new();
    for attachment in &said.attachments {
        if let SaidAttachment::LocalFile { id, local_path, .. } = attachment {
            let source = validate_local_path(&inbox, local_path)?;
            let destination = store.join(format!("{id}.bin"));
            if std::fs::hard_link(&source, &destination).is_err() {
                remove_promoted_files(&created);
                return Err(GateError::store());
            }
            created.push(destination);
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

fn validate_local_path(root: &Path, relative: &str) -> Result<PathBuf, GateError> {
    let joined = root.join(relative);
    let link_meta = joined
        .symlink_metadata()
        .map_err(|_| GateError::new(ErrorCode::BadRequest))?;
    if !link_meta.file_type().is_file() {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    let canonical = joined
        .canonicalize()
        .map_err(|_| GateError::new(ErrorCode::BadRequest))?;
    if !canonical.starts_with(root) {
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

fn image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_image_classification_uses_actual_bytes() {
        assert!(is_text("text/html; charset=utf-8"));
        assert!(is_text("application/json"));
        assert_eq!(
            image_media_type(b"\x89PNG\r\n\x1a\nrest"),
            Some("image/png")
        );
        assert_eq!(
            image_media_type(&[0xff, 0xd8, 0xff, 0x00]),
            Some("image/jpeg")
        );
        assert_eq!(image_media_type(b"not an image"), None);
    }

    #[test]
    fn local_path_accepts_a_regular_file_without_metadata_comparisons() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance/file.bin");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"actual bytes decide").unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert_eq!(
            validate_local_path(&root, "instance/file.bin").unwrap(),
            path.canonicalize().unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_path_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"hello").unwrap();
        symlink(&target, dir.path().join("link")).unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert!(validate_local_path(&root, "link").is_err());
    }
}
