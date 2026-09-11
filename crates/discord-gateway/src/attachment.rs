//! Gateway-side attachment download into the core-provided local inbox.

use std::path::{Path, PathBuf};
use std::time::Duration;

use opencrab_gate_client::wire::Attachment;
use tokio::io::AsyncWriteExt;

use crate::map::IncomingAttachment;

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(200);

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
        _origin: &str,
        source: &IncomingAttachment,
    ) -> anyhow::Result<Attachment> {
        let source_url = reqwest::Url::parse(&source.url)?;
        if !is_allowed_source(&source_url) {
            anyhow::bail!("attachment source is not an allowed Discord CDN URL");
        }
        let attachment_id = uuid::Uuid::new_v4().to_string();
        let relative = PathBuf::from(instance_id).join(format!("{attachment_id}.bin"));
        let final_path = self.root.join(&relative);
        let parent = final_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("attachment path has no parent"))?;
        tokio::fs::create_dir_all(parent).await?;
        #[cfg(unix)]
        tokio::fs::set_permissions(parent, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .await?;
        let part_path = parent.join(format!(".{attachment_id}.part"));

        let media_type = self
            .download_with_retries(&source.url, &part_path, &final_path)
            .await?;
        Ok(Attachment::LocalFile {
            id: attachment_id,
            name: safe_name(&source.filename),
            media_type: media_type.or_else(|| source.content_type.clone()),
            size: 0,
            sha256: String::new(),
            local_path: path_to_wire(&relative)?,
        })
    }

    async fn download_with_retries(
        &self,
        url: &str,
        part_path: &Path,
        final_path: &Path,
    ) -> anyhow::Result<Option<String>> {
        let mut last_error = None;
        for attempt in 1..=DOWNLOAD_ATTEMPTS {
            let _ = tokio::fs::remove_file(part_path).await;
            match self.download_to_part(url, part_path, final_path).await {
                Ok(media_type) => return Ok(media_type),
                Err(DownloadFailure::Fatal(error)) => {
                    let _ = tokio::fs::remove_file(part_path).await;
                    let _ = tokio::fs::remove_file(final_path).await;
                    return Err(error);
                }
                Err(DownloadFailure::Retryable(error)) => last_error = Some(error),
            }
            if attempt < DOWNLOAD_ATTEMPTS {
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
        let _ = tokio::fs::remove_file(part_path).await;
        let _ = tokio::fs::remove_file(final_path).await;
        Err(last_error.expect("at least one download attempt"))
    }

    async fn download_to_part(
        &self,
        url: &str,
        part_path: &Path,
        final_path: &Path,
    ) -> Result<Option<String>, DownloadFailure> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| DownloadFailure::Retryable(error.into()))?;
        let mut response = response
            .error_for_status()
            .map_err(|error| DownloadFailure::Fatal(error.into()))?;
        let media_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).trim().to_string())
            .filter(|value| !value.is_empty());
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(part_path)
            .await
            .map_err(|error| DownloadFailure::Fatal(error.into()))?;
        loop {
            let chunk = tokio::time::timeout(IDLE_TIMEOUT, response.chunk())
                .await
                .map_err(|_| {
                    DownloadFailure::Retryable(anyhow::anyhow!("attachment transfer idle timeout"))
                })?
                .map_err(|error| DownloadFailure::Retryable(error.into()))?;
            let Some(chunk) = chunk else { break };
            file.write_all(&chunk)
                .await
                .map_err(|error| DownloadFailure::Fatal(error.into()))?;
        }
        file.sync_all()
            .await
            .map_err(|error| DownloadFailure::Fatal(error.into()))?;
        drop(file);
        tokio::fs::rename(part_path, final_path)
            .await
            .map_err(|error| DownloadFailure::Fatal(error.into()))?;
        Ok(media_type)
    }

    pub async fn remove(&self, attachment: &Attachment) {
        if let Attachment::LocalFile { local_path, .. } = attachment {
            let _ = tokio::fs::remove_file(self.root.join(local_path)).await;
        }
    }
}

enum DownloadFailure {
    Retryable(anyhow::Error),
    Fatal(anyhow::Error),
}

fn is_allowed_source(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some("cdn.discordapp.com") | Some("media.discordapp.net")
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt as _;

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

    #[tokio::test]
    async fn retries_a_broken_transfer_and_uses_the_response_representation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for response in [
                "HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\nbroken",
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 11\r\n\r\nactual-data",
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request).await.unwrap();
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.part");
        let final_path = dir.path().join("file.bin");
        let spool = AttachmentSpool {
            root: dir.path().to_path_buf(),
            client: reqwest::Client::new(),
        };
        let media_type = spool
            .download_with_retries(&format!("http://{address}/image"), &part, &final_path)
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(media_type.as_deref(), Some("image/png"));
        assert_eq!(std::fs::read(final_path).unwrap(), b"actual-data");
    }
}
