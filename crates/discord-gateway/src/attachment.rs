//! Gateway-side attachment download into the core-provided local inbox.

use std::path::{Path, PathBuf};
use std::time::Duration;

use opencrab_gate_client::wire::Attachment;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::map::IncomingAttachment;

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct AttachmentSpool {
    root: PathBuf,
    client: reqwest::Client,
}

impl AttachmentSpool {
    pub fn new(root: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        #[cfg(unix)]
        std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 3 || !is_allowed_source(attempt.url()) {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()?;
        Ok(Self { root, client })
    }

    pub async fn download(
        &self,
        instance_id: &str,
        origin: &str,
        source: &IncomingAttachment,
    ) -> anyhow::Result<Attachment> {
        let source_url = reqwest::Url::parse(&source.url)?;
        if !is_allowed_source(&source_url) {
            anyhow::bail!("attachment source is not an allowed Discord CDN URL");
        }
        let origin_hash = lower_hex(&Sha256::digest(origin.as_bytes()));
        let attachment_id = uuid::Uuid::new_v4().to_string();
        let relative = PathBuf::from(instance_id)
            .join(origin_hash)
            .join(format!("{attachment_id}.bin"));
        let final_path = self.root.join(&relative);
        let parent = final_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("attachment path has no parent"))?;
        tokio::fs::create_dir_all(parent).await?;
        #[cfg(unix)]
        tokio::fs::set_permissions(parent, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .await?;
        let part_path = parent.join(format!(".{attachment_id}.part"));

        let result = self.download_to_part(source, &part_path, &final_path).await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&part_path).await;
            let _ = tokio::fs::remove_file(&final_path).await;
        }
        let sha256 = result?;
        Ok(Attachment::LocalFile {
            id: attachment_id,
            name: safe_name(&source.filename),
            media_type: source.content_type.clone(),
            size: source.size,
            sha256,
            local_path: path_to_wire(&relative)?,
        })
    }

    async fn download_to_part(
        &self,
        source: &IncomingAttachment,
        part_path: &Path,
        final_path: &Path,
    ) -> anyhow::Result<String> {
        let mut response = self
            .client
            .get(&source.url)
            .send()
            .await?
            .error_for_status()?;
        if let Some(length) = response.content_length() {
            verify_size(length, source.size)?;
        }
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(part_path).await?;
        let mut actual = 0_u64;
        let mut hash = Sha256::new();
        loop {
            let chunk = tokio::time::timeout(IDLE_TIMEOUT, response.chunk())
                .await
                .map_err(|_| anyhow::anyhow!("attachment transfer idle timeout"))??;
            let Some(chunk) = chunk else { break };
            actual = actual
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| anyhow::anyhow!("attachment size overflow"))?;
            if actual > source.size {
                verify_size(actual, source.size)?;
            }
            hash.update(&chunk);
            file.write_all(&chunk).await?;
        }
        verify_size(actual, source.size)?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(part_path, final_path).await?;
        Ok(lower_hex(&hash.finalize()))
    }

    pub async fn remove(&self, attachment: &Attachment) {
        if let Attachment::LocalFile { local_path, .. } = attachment {
            let _ = tokio::fs::remove_file(self.root.join(local_path)).await;
        }
    }
}

fn is_allowed_source(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some("cdn.discordapp.com") | Some("media.discordapp.net")
        )
}

fn verify_size(actual: u64, declared: u64) -> anyhow::Result<()> {
    if actual != declared {
        anyhow::bail!("attachment size differs from platform metadata");
    }
    Ok(())
}

fn safe_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_control() || c == '/' || c == '\\' {
                '_'
            } else {
                c
            }
        })
        .take(255)
        .collect();
    if safe.is_empty() {
        "attachment".into()
    } else {
        safe
    }
}

fn path_to_wire(path: &Path) -> anyhow::Result<String> {
    let parts: Option<Vec<&str>> = path.iter().map(|part| part.to_str()).collect();
    Ok(parts
        .ok_or_else(|| anyhow::anyhow!("attachment path is not UTF-8"))?
        .join("/"))
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
    fn size_is_integrity_equality_not_an_opencrab_cap() {
        assert!(verify_size(u64::MAX, u64::MAX).is_ok());
        assert!(verify_size(100, 99).is_err());
        assert!(verify_size(98, 99).is_err());
    }

    #[test]
    fn accepts_only_discord_https_cdn_sources() {
        assert!(is_allowed_source(
            &reqwest::Url::parse("https://cdn.discordapp.com/attachments/a/b/file").unwrap()
        ));
        assert!(is_allowed_source(
            &reqwest::Url::parse("https://media.discordapp.net/attachments/a/b/file").unwrap()
        ));
        assert!(!is_allowed_source(
            &reqwest::Url::parse("https://127.0.0.1/private").unwrap()
        ));
        assert!(!is_allowed_source(
            &reqwest::Url::parse("http://cdn.discordapp.com/attachments/a/b/file").unwrap()
        ));
    }

    #[test]
    fn filename_is_metadata_not_a_path() {
        assert_eq!(safe_name("../bad\\name.html"), ".._bad_name.html");
        assert_eq!(safe_name(""), "attachment");
    }
}
