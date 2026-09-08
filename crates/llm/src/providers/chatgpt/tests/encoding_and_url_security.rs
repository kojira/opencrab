/// マルチモーダル（画像）ユーザーメッセージが Responses API 形式
/// （input_text / input_image・image_url は文字列）で組まれること。
/// 以前は Chat Completions 形式（type:image_url, image_url:{url}）で、
/// codex/responses バックエンドでは画像が無視/拒否されていた。
#[test]
fn test_message_content_value_multimodal_uses_responses_format() {
    let content = Some(MessageContent::Multi(vec![
        ContentPart::Text {
            text: "この画像を見て".to_string(),
        },
        ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: "https://cdn.discordapp.com/x.png".to_string(),
                detail: Some("auto".to_string()),
            },
        },
    ]));
    let v = ChatGptProvider::message_content_value(&content).unwrap();
    let arr = v.as_array().expect("content is an array");
    assert_eq!(arr[0]["type"], "input_text");
    assert_eq!(arr[0]["text"], "この画像を見て");
    assert_eq!(arr[1]["type"], "input_image");
    // image_url は文字列（オブジェクトではない）。
    assert_eq!(arr[1]["image_url"], "https://cdn.discordapp.com/x.png");
    assert_eq!(arr[1]["detail"], "auto");
}

/// RFC 4648 のテストベクタで base64 符号化を検証（壊れると画像が壊れる）。
#[test]
fn test_base64_encode_vectors() {
    assert_eq!(base64_encode(b""), "");
    assert_eq!(base64_encode(b"f"), "Zg==");
    assert_eq!(base64_encode(b"fo"), "Zm8=");
    assert_eq!(base64_encode(b"foo"), "Zm9v");
    assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
    assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    // 非 ASCII バイト（0xFF 等）も正しく符号化。
    assert_eq!(base64_encode(&[0xff, 0xff, 0xff]), "////");
    assert_eq!(base64_encode(&[0x00]), "AA==");
}

#[test]
fn test_is_global_ip_rejects_internal() {
    use std::net::IpAddr;
    let bad = [
        "127.0.0.1",
        "10.0.0.1",
        "192.168.1.1",
        "172.16.0.1",
        "169.254.169.254", // クラウドメタデータ
        "100.64.0.1",      // CGNAT
        "0.0.0.0",
        "::1",
        "::ffff:127.0.0.1", // v4-mapped loopback
        "::ffff:169.254.169.254",
        "fe80::1",
        "fc00::1",
        "::7f00:1",           // ::127.0.0.1 (IPv4-compatible, deprecated)
        "2002:7f00:1::",      // 6to4 embedding 127.0.0.1
        "2001:0:0:0:0:0:0:1", // Teredo 2001:0000::/32
        "64:ff9b::7f00:1",    // NAT64 embedding 127.0.0.1
    ];
    for s in bad {
        let ip: IpAddr = s.parse().unwrap();
        assert!(!is_global_ip(ip), "{s} should be rejected");
    }
    let good = ["8.8.8.8", "1.1.1.1", "2001:4860:4860::8888"];
    for s in good {
        let ip: IpAddr = s.parse().unwrap();
        assert!(is_global_ip(ip), "{s} should be allowed");
    }
}

#[tokio::test]
async fn test_validate_public_url_rejects_localhost_and_scheme() {
    // 明示 IP のループバックは解決不要で弾ける。
    let u = reqwest::Url::parse("http://127.0.0.1:8080/x.png").unwrap();
    assert!(validate_public_url(&u).await.is_err());
    // 非 http スキーム。
    let u = reqwest::Url::parse("ftp://example.com/x.png").unwrap();
    assert!(validate_public_url(&u).await.is_err());
}

#[test]
fn test_guess_image_mime() {
    assert_eq!(guess_image_mime("https://x/y.png"), "image/png");
    assert_eq!(guess_image_mime("https://x/y.JPG?ex=1&is=2"), "image/jpeg");
    assert_eq!(guess_image_mime("https://x/y.webp"), "image/webp");
    assert_eq!(guess_image_mime("https://x/y.gif#frag"), "image/gif");
    assert_eq!(guess_image_mime("https://x/noext"), "image/png");
}

/// data: URI と非 http はダウンロードを試みず素通しすること（ネットワーク不要）。
#[tokio::test]
async fn test_inline_one_image_skips_data_and_non_http() {
    let p = ChatGptProvider::new();
    let mut data = ImageUrl {
        url: "data:image/png;base64,AAAA".to_string(),
        detail: None,
    };
    p.inline_one_image(&mut data).await;
    assert_eq!(data.url, "data:image/png;base64,AAAA");

    let mut rel = ImageUrl {
        url: "file/local.png".to_string(),
        detail: None,
    };
    p.inline_one_image(&mut rel).await;
    assert_eq!(rel.url, "file/local.png");
}
