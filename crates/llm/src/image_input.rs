//! 画像入力の取得と正規化（D-1060）。
//!
//! LLM へ渡す画像を長辺 [`MAX_LONG_EDGE`] px 以内・[`MAX_OUTPUT_BYTES`] 以内の
//! data URL にそろえる。URL 取得は SSRF 対策（公開 IP のみ・解決 IP 固定・
//! リダイレクト無効）を必ず通す。

use std::io::Cursor;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use image::{DynamicImage, ImageFormat};

/// 送信する画像の長辺上限（px）。
pub const MAX_LONG_EDGE: u32 = 1568;
/// 送信する画像バイト数の上限。超えたら JPEG で再エンコードする。
pub const MAX_OUTPUT_BYTES: usize = 3_750_000;
/// 取得する元画像の上限。
pub const MAX_SOURCE_BYTES: usize = 20 * 1024 * 1024;
const JPEG_QUALITY: u8 = 85;

/// 正規化済み画像。`data_url` が送信用、寸法は元画像と送信画像。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedImage {
    pub data_url: String,
    pub orig_w: u32,
    pub orig_h: u32,
    pub w: u32,
    pub h: u32,
    pub resized: bool,
}

fn mime_of(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        _ => "image/webp",
    }
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

fn encode(img: &DynamicImage, format: ImageFormat) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    if format == ImageFormat::Jpeg {
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
        DynamicImage::ImageRgb8(img.to_rgb8())
            .write_with_encoder(encoder)
            .context("failed to encode jpeg")?;
    } else {
        img.write_to(&mut Cursor::new(&mut buf), format)
            .context("failed to encode image")?;
    }
    Ok(buf)
}

/// 画像バイト列を判定・縮小して data URL にする。png/jpeg/gif/webp 以外はエラー。
/// gif は先頭フレームを使う。縮小不要かつ上限以内なら元バイトをそのまま使う。
pub fn normalize_image_bytes(bytes: &[u8]) -> Result<NormalizedImage> {
    let format = image::guess_format(bytes).context("unrecognized image format")?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
    ) {
        anyhow::bail!("unsupported image format: {format:?}");
    }
    let img =
        image::load_from_memory_with_format(bytes, format).context("failed to decode image")?;
    let (orig_w, orig_h) = (img.width(), img.height());
    let resized = orig_w.max(orig_h) > MAX_LONG_EDGE;
    if !resized && bytes.len() <= MAX_OUTPUT_BYTES {
        return Ok(NormalizedImage {
            data_url: data_url(mime_of(format), bytes),
            orig_w,
            orig_h,
            w: orig_w,
            h: orig_h,
            resized: false,
        });
    }
    let out = if resized {
        img.resize(
            MAX_LONG_EDGE,
            MAX_LONG_EDGE,
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    // 縮小後は JPEG 元なら JPEG、それ以外は PNG。上限超えなら JPEG に落とす。
    let mut out_format = if format == ImageFormat::Jpeg {
        ImageFormat::Jpeg
    } else {
        ImageFormat::Png
    };
    let mut encoded = encode(&out, out_format)?;
    if encoded.len() > MAX_OUTPUT_BYTES && out_format != ImageFormat::Jpeg {
        out_format = ImageFormat::Jpeg;
        encoded = encode(&out, out_format)?;
    }
    Ok(NormalizedImage {
        data_url: data_url(mime_of(out_format), &encoded),
        orig_w,
        orig_h,
        w: out.width(),
        h: out.height(),
        resized,
    })
}

fn decode_data_url(url: &str) -> Result<Vec<u8>> {
    let rest = url.strip_prefix("data:").context("not a data url")?;
    let (meta, payload) = rest.split_once(',').context("malformed data url")?;
    anyhow::ensure!(meta.ends_with(";base64"), "data url must be base64");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .context("invalid base64 in data url")?;
    anyhow::ensure!(
        bytes.len() <= MAX_SOURCE_BYTES,
        "image too large ({} bytes, max 20MB)",
        bytes.len()
    );
    Ok(bytes)
}

/// 画像 URL（http/https または `data:`）を取得して正規化する。
///
/// SSRF 対策: ホストを解決して全 IP が公開アドレスであることを確認し、その IP に
/// 固定して接続する。リダイレクトは無効。元画像は 20MB まで。
pub async fn fetch_image_data_url(url: &str) -> Result<NormalizedImage> {
    let url = url.trim();
    if url.starts_with("data:") {
        return normalize_image_bytes_blocking(decode_data_url(url)?).await;
    }
    let parsed = reqwest::Url::parse(url).context("invalid image url")?;
    let host = parsed
        .host_str()
        .context("image url has no host")?
        .to_string();
    let pinned = validate_public_url(&parsed).await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .resolve(&host, pinned)
        .build()
        .context("failed to build image http client")?;
    let resp = client
        .get(url)
        .send()
        .await
        .context("image download request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("image download HTTP {}", resp.status());
    }
    if let Some(len) = resp.content_length() {
        if len > MAX_SOURCE_BYTES as u64 {
            anyhow::bail!("image too large ({len} bytes, max 20MB)");
        }
    }
    let bytes = read_body_capped(resp, MAX_SOURCE_BYTES).await?;
    normalize_image_bytes_blocking(bytes).await
}

/// 本文を読みながら上限を効かせる（Content-Length 無しでも全量を溜めない）。
async fn read_body_capped(mut resp: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.context("failed to read image body")? {
        if buf.len() + chunk.len() > max {
            anyhow::bail!("image too large (over {max} bytes, max 20MB)");
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// デコード・縮小は CPU 処理なので async worker を塞がないよう blocking スレッドで行う。
async fn normalize_image_bytes_blocking(bytes: Vec<u8>) -> Result<NormalizedImage> {
    tokio::task::spawn_blocking(move || normalize_image_bytes(&bytes))
        .await
        .context("image normalize task failed")?
}

/// http(s) URL のホストを解決し、全解決 IP が公開アドレスであることを確認する（SSRF 対策）。
/// 接続に使う（検証済みの）SocketAddr を返す。1つでも非公開アドレスに解決したら拒否。
pub async fn validate_public_url(parsed: &reqwest::Url) -> anyhow::Result<std::net::SocketAddr> {
    use anyhow::Context;
    match parsed.scheme() {
        "http" | "https" => {}
        other => anyhow::bail!("unsupported url scheme for image fetch: {other}"),
    }
    let host = parsed.host_str().context("image url has no host")?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("failed to resolve host {host}"))?
        .collect();
    let first = *addrs
        .first()
        .context("host did not resolve to any address")?;
    for addr in &addrs {
        if !is_global_ip(addr.ip()) {
            anyhow::bail!(
                "refusing to fetch image from non-public address ({})",
                addr.ip()
            );
        }
    }
    Ok(first)
}

/// IP が公開（グローバル）アドレスか。ループバック/プライベート/リンクローカル
/// （169.254.169.254 のメタデータ含む）/CGNAT/ユニークローカル等は非公開として弾く。
/// `std::net` の `is_global` は unstable のため、既知の非公開レンジを直接判定する。
pub fn is_global_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local() // 169.254.0.0/16（メタデータ 169.254.169.254 含む）
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || (o[0] == 100 && (o[1] & 0xc0) == 64)) // carrier-grade NAT range
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped（::ffff:a.b.c.d）で内部アドレスへ回避されないよう展開して判定。
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_global_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
                // 埋め込み v4 で内部アドレスを指しうる遷移レンジは一律非公開扱い
                // （::a.b.c.d 互換 / 6to4 / Teredo / NAT64。正当な画像ホストは通常来ない）。
                || (s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0) // ::/96 IPv4-compatible
                || s[0] == 0x2002 // 6to4 2002::/16
                || (s[0] == 0x2001 && s[1] == 0x0000) // Teredo 2001:0000::/32
                || (s[0] == 0x0064 && s[1] == 0xff9b)) // NAT64 64:ff9b::/96
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        }));
        encode(&img, ImageFormat::Png).unwrap()
    }

    #[test]
    fn normalize_image_bytes_shrinks_long_edge_to_limit() {
        let out = normalize_image_bytes(&png(3000, 2000)).unwrap();
        assert!(out.resized);
        assert_eq!((out.orig_w, out.orig_h), (3000, 2000));
        assert_eq!(out.w, MAX_LONG_EDGE);
        assert!((1044..=1046).contains(&out.h), "h = {}", out.h);
        assert!(out.data_url.starts_with("data:image/"));
        let bytes = decode_data_url(&out.data_url).unwrap();
        assert!(bytes.len() <= MAX_OUTPUT_BYTES);
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (out.w, out.h));
    }

    #[test]
    fn normalize_image_bytes_keeps_small_image_bytes() {
        let src = png(40, 30);
        let out = normalize_image_bytes(&src).unwrap();
        assert!(!out.resized);
        assert_eq!((out.w, out.h), (40, 30));
        assert!(out.data_url.starts_with("data:image/png;base64,"));
        assert_eq!(decode_data_url(&out.data_url).unwrap(), src);
    }

    #[test]
    fn normalize_image_bytes_rejects_text() {
        assert!(normalize_image_bytes(b"hello, this is not an image").is_err());
    }

    #[tokio::test]
    async fn fetch_image_data_url_accepts_data_url_and_rejects_private_host() {
        let src = png(10, 10);
        let url = data_url("image/png", &src);
        let out = fetch_image_data_url(&url).await.unwrap();
        assert_eq!(decode_data_url(&out.data_url).unwrap(), src);
        assert!(fetch_image_data_url("http://127.0.0.1/a.png")
            .await
            .is_err());
        assert!(fetch_image_data_url("file:///etc/passwd").await.is_err());
    }

    #[tokio::test]
    async fn chunked_body_over_cap_is_rejected_while_streaming() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = [0u8; 1024];
            let _ = sock.read(&mut req).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await;
            let chunk = vec![b'x'; 1024];
            for _ in 0..64 {
                let head = format!("{:x}\r\n", chunk.len());
                if sock.write_all(head.as_bytes()).await.is_err()
                    || sock.write_all(&chunk).await.is_err()
                    || sock.write_all(b"\r\n").await.is_err()
                {
                    return;
                }
            }
            let _ = sock.write_all(b"0\r\n\r\n").await;
        });
        let resp = reqwest::get(format!("http://{addr}/")).await.unwrap();
        assert!(resp.content_length().is_none());
        let err = read_body_capped(resp, 4096).await.unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }
}
