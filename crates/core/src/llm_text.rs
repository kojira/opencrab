//! LLM 入出力テキストの小さな共通ユーティリティ。

/// LLM 応答テキストからマークダウンコードフェンスを剥がす。
///
/// `memory_index` / `daily_log_indexer` / `evaluator` で共用。
pub fn strip_code_fences(text: &str) -> &str {
    text.trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
}

/// 画像として扱う URL の拡張子（小文字・先頭ドット込み）。
///
/// 「この URL は画像か」という**判断**をここ（core）に集約する。ゲートウェイ
/// （Nostr / Discord 等）は本文からこの関数で URL を拾って `image_urls` に載せるだけで、
/// 何を画像と見なすかの基準を各ゲートに持たせない（gateways-deliver-core-decides）。
///
/// 下流（Claude vision）が URL パススルーで**受理する形式**に絞る。svg/bmp/avif/heic/heif は
/// 受理されず、hermit（openai 形式）の image_url パススルーではリクエストごと失敗しうるため
/// 載せない。対応形式が増えたらここに足す（下流の実能力に合わせる）。
const IMAGE_URL_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".gif", ".webp"];

/// テキスト本文から画像 URL を抽出する（出現順・重複除去）。
///
/// Nostr のノート本文のように、画像が構造化された添付ではなく本文中の URL 文字列として
/// 載る経路のための共通抽出器。判定は**拡張子ベース**（[`IMAGE_URL_EXTENSIONS`]）に
/// 限る:
///
/// - `http://` / `https://` で始まる URL だけを対象にする。
/// - クエリ（`?`）・フラグメント（`#`）を除いたパス末尾が画像拡張子で終わるものを拾う。
///   クエリ付き（`https://host/a.png?x=1`）でも拡張子はパス側で判定するので通る。
/// - Markdown 記法 `![alt](url)` や括弧・句読点で囲まれた URL は、前後の記号を剥がして拾う。
///
/// 拡張子の無い URL（content-addressed な blossom URL 等）は**あえて画像と推測しない**。
/// 中身を HEAD で叩いて判定するような暗黙のフォールバックは入れない（取れないなら
/// image_urls に載らない＝見えないことが見える）。
pub fn extract_image_urls(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    // 本文中に現れる各 `http(s)://` を順に取り出す。空白での分割に頼らない: 日本語本文では
    // URL の直後に空白なしで全角の句読点や本文（`…c.webp。続き`）が続くため、URL の終端は
    // 「URL に使える文字か」で見極める。RFC 3986 で URL に使える文字は ASCII のごく一部だけで、
    // 日本語などの非 ASCII 文字はそのままでは URL に現れない（現れるなら percent-encode 済み）。
    // よって URL 使用可能文字が続く限り取り、最初にそうでない文字（空白・全角句読点・本文）で切る。
    //
    // http:// と https:// の**早い方**から取る。`or_else` で https を優先すると、手前に
    // http:// の画像 URL があっても後ろの https:// へ飛んで手前を黙って取りこぼす。
    while let Some(rel) = earliest_scheme(rest) {
        let after = &rest[rel..];
        // URL に使える文字が続く範囲を切り出す。
        let end = after.find(|c: char| !is_url_char(c)).unwrap_or(after.len());
        let candidate = &after[..end];
        // 次の探索はこの URL の後ろから（無限ループ防止に最低 1 バイトは進める）。
        rest = &after[end.max(1)..];

        // 末尾に付いた文末記号・閉じ括弧を落とす。`)` 等は URL にも使えるため範囲には
        // 含めたが、`![alt](url)` や `(url)` の閉じ括弧・句読点は実体の URL ではない。
        let url = candidate.trim_end_matches(|c| {
            matches!(
                c,
                ')' | ']' | '}' | '>' | '"' | '\'' | ',' | '.' | ';' | ':' | '!' | '?'
            )
        });
        if url.is_empty() {
            continue;
        }
        // クエリ・フラグメントを除いたパス部分で拡張子を判定する。
        let path = url.split(['?', '#']).next().unwrap_or(url);
        let path_lower = path.to_ascii_lowercase();
        if IMAGE_URL_EXTENSIONS
            .iter()
            .any(|ext| path_lower.ends_with(ext))
        {
            let url = url.to_string();
            if !out.contains(&url) {
                out.push(url);
            }
        }
    }
    out
}

/// `http://` / `https://` のうち**先に現れる方**のバイト位置。両方無ければ `None`。
fn earliest_scheme(s: &str) -> Option<usize> {
    match (s.find("http://"), s.find("https://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// RFC 3986 が URL に許す ASCII 文字か（scheme 以降の 1 文字を想定）。
///
/// 非 ASCII（日本語等）は false を返すので、本文中の URL は次の日本語文字や
/// 全角句読点で自然に切れる。空白も false。
fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
}

/// 文字数上限で切り詰め、超過時は省略記号を付ける（UTF-8 安全）。
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_code_fences_variants() {
        assert_eq!(strip_code_fences("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_code_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fences("```\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fences("  {\"a\":1}  "), "{\"a\":1}");
    }

    #[test]
    fn truncate_chars_multibyte_safe() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abcdef", 3), "abc…");
        assert_eq!(truncate_chars("あいうえお", 2), "あい…");
    }

    #[test]
    fn extract_image_urls_basic_extensions() {
        assert_eq!(
            extract_image_urls("見て https://cdn.example/a.png"),
            vec!["https://cdn.example/a.png".to_string()]
        );
        assert_eq!(
            extract_image_urls("https://x/1.JPG https://x/2.WebP"),
            vec![
                "https://x/1.JPG".to_string(),
                "https://x/2.WebP".to_string()
            ]
        );
    }

    #[test]
    fn extract_image_urls_query_and_fragment() {
        // クエリ・フラグメント付きでもパス末尾の拡張子で拾う。
        assert_eq!(
            extract_image_urls("https://host/img.jpeg?ex=deadbeef&s=1"),
            vec!["https://host/img.jpeg?ex=deadbeef&s=1".to_string()]
        );
        assert_eq!(
            extract_image_urls("https://host/pic.png#frag"),
            vec!["https://host/pic.png#frag".to_string()]
        );
    }

    #[test]
    fn extract_image_urls_strips_surrounding_punctuation() {
        // Markdown 記法・括弧・句読点で囲まれていても剥がして拾う。
        assert_eq!(
            extract_image_urls("これ![alt](https://host/a.png) だよ。"),
            vec!["https://host/a.png".to_string()]
        );
        assert_eq!(
            extract_image_urls("(https://host/b.gif)、"),
            vec!["https://host/b.gif".to_string()]
        );
        // 全角句読点が空白なしで続く日本語本文（Nostr で最も普通の形）。
        assert_eq!(
            extract_image_urls("写真です https://host/c.webp。続き"),
            vec!["https://host/c.webp".to_string()]
        );
    }

    #[test]
    fn extract_image_urls_ignores_non_images() {
        // 拡張子の無い URL（content-addressed な blossom 等）や非画像は推測で拾わない。
        assert!(extract_image_urls("https://example.com/page").is_empty());
        assert!(extract_image_urls("https://blossom.example/deadbeefcafe").is_empty());
        assert!(extract_image_urls("https://host/doc.pdf 文章").is_empty());
        assert!(extract_image_urls("画像なしの本文").is_empty());
    }

    #[test]
    fn extract_image_urls_excludes_unsupported_formats() {
        // 下流（Claude vision）が受理しない形式は載せない（svg/bmp/avif/heic/heif）。
        assert!(extract_image_urls("https://host/a.svg").is_empty());
        assert!(extract_image_urls("https://host/a.bmp").is_empty());
        assert!(extract_image_urls("https://host/a.avif").is_empty());
        assert!(extract_image_urls("https://host/a.heic").is_empty());
    }

    #[test]
    fn extract_image_urls_mixed_http_and_https_no_drop() {
        // http:// が手前・https:// が後続でも、手前の http:// を取りこぼさない
        // （早い方から順に取る）。
        assert_eq!(
            extract_image_urls("http://a.example/1.png と https://b.example/2.jpg"),
            vec![
                "http://a.example/1.png".to_string(),
                "https://b.example/2.jpg".to_string()
            ]
        );
    }

    #[test]
    fn extract_image_urls_dedups_preserving_order() {
        assert_eq!(
            extract_image_urls("https://x/a.png https://x/b.png https://x/a.png"),
            vec!["https://x/a.png".to_string(), "https://x/b.png".to_string()]
        );
    }
}
