use super::*;

/// バイト列を標準 base64（`+/`・パディングあり）で符号化する。data URI 用。
pub(super) fn base64_encode(data: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHA[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHA[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// http(s) URL のホストを解決し、全解決 IP が公開アドレスであることを確認する（SSRF 対策）。
/// 接続に使う（検証済みの）SocketAddr を返す。1つでも非公開アドレスに解決したら拒否。
pub(super) async fn validate_public_url(
    parsed: &reqwest::Url,
) -> anyhow::Result<std::net::SocketAddr> {
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
pub(super) fn is_global_ip(ip: std::net::IpAddr) -> bool {
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
                || (o[0] == 100 && (o[1] & 0xc0) == 64)) // 100.64.0.0/10 CGNAT
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

/// URL の拡張子から画像 MIME を推測する（Content-Type が無い/不正なときのフォールバック）。
pub(super) fn guess_image_mime(url: &str) -> String {
    // クエリ以降を落として拡張子を見る。
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png", // 不明時は png 扱い（多くの backend が受ける）
    }
    .to_string()
}

impl ChatGptProvider {
    /// リクエスト中の http(s) 画像URLを取得して base64 data URI に置き換えた
    /// コピーを返す。
    ///
    /// Discord の添付画像URL（cdn.discordapp.com）は、LLM バックエンドから直接
    /// 取得しようとすると bot 判定・署名付きURLの都合で弾かれることがある。こちらで
    /// 取得して data URI として埋め込めば、外部取得に依存せず確実に読ませられる。
    /// 既に data URI のもの・非 http のものは触らない。取得失敗時は元のURLのまま
    /// 残す（送信自体は試みる）。
    pub(super) async fn inline_remote_images(&self, request: &ChatRequest) -> ChatRequest {
        let mut req = request.clone();
        // 直近のユーザーターン以降の画像だけを対象にする。毎ターン全履歴を落とすと
        // 「画像数 × タイムアウト」の遅延になり、過去ターンの署名付きURLは失効して
        // いて無駄な往復になりやすいため（現在ターンの画像だけ読めれば足りる）。
        let Some(start) = req.messages.iter().rposition(|m| m.role == Role::User) else {
            return req;
        };
        for msg in req.messages[start..].iter_mut() {
            match &mut msg.content {
                Some(MessageContent::Multi(parts)) => {
                    for part in parts.iter_mut() {
                        if let ContentPart::ImageUrl { image_url } = part {
                            self.inline_one_image(image_url).await;
                        }
                    }
                }
                Some(MessageContent::Image { image_url, .. }) => {
                    self.inline_one_image(image_url).await;
                }
                _ => {}
            }
        }
        req
    }

    pub(super) async fn inline_one_image(&self, image_url: &mut ImageUrl) {
        let url = image_url.url.trim();
        if url.starts_with("data:") {
            return; // 既に data URI
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return; // 相対/未知スキームは触らない
        }
        match self.download_image_data_uri(url).await {
            Ok(data_uri) => image_url.url = data_uri,
            Err(e) => {
                tracing::warn!(error = %e, "failed to inline image as data URI; keeping original url")
            }
        }
    }

    /// 画像URLをダウンロードして `data:<mime>;base64,<...>` を返す。上限 10MB。
    ///
    /// SSRF 対策: 取得前にホストを解決し、全解決 IP が公開アドレスであることを確認、
    /// その IP に固定して接続する（DNS rebinding 対策）。リダイレクトは無効化し、
    /// 内部URLへ飛ばされないようにする。画像URLはユーザー（Discord メッセージ）由来の
    /// 信頼できない入力なので、この検証を必ず通す。
    async fn download_image_data_uri(&self, url: &str) -> Result<String> {
        const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

        let parsed = reqwest::Url::parse(url).context("invalid image url")?;
        let host = parsed
            .host_str()
            .context("image url has no host")?
            .to_string();
        let pinned = validate_public_url(&parsed).await?;

        // 検証済み IP に固定・リダイレクト無効・短めのタイムアウトの専用クライアント。
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
        // Content-Length があればダウンロード前に上限判定（メモリ濫用の抑制）。
        if let Some(len) = resp.content_length() {
            if len > MAX_IMAGE_BYTES {
                anyhow::bail!("image too large ({len} bytes, max 10MB)");
            }
        }
        let header_mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
            .filter(|c| c.starts_with("image/"));
        let bytes = resp.bytes().await.context("failed to read image body")?;
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            anyhow::bail!("image too large ({} bytes, max 10MB)", bytes.len());
        }
        let mime = header_mime.unwrap_or_else(|| guess_image_mime(url));
        Ok(format!("data:{mime};base64,{}", base64_encode(&bytes)))
    }
}
