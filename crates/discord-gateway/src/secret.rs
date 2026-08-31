//! bot token。process env から 1 回だけ読み、直後に process env から消す（設計 §1.3・§5）。
//! argv / V3 frame / config_b64 / operation payload/result / log / workspace 平文へは出さない。

/// bot token を渡す env 変数。setup（supervisor / secret provider）が注入する。
pub const TOKEN_ENV: &str = "DISCORD_BOT_TOKEN";

/// 起動時に 1 回読む。直後に process env から消す。空は「トークンなし」。
///
/// 消すことで、以後子プロセス spawn や誤ログで token が漏れる経路を塞ぐ（Nostr の
/// `take_watch_secret` と同形）。real transport は返り値を保持して serenity へだけ渡す。
pub fn take_bot_token() -> Option<String> {
    let value = std::env::var(TOKEN_ENV).ok().filter(|s| !s.is_empty());
    std::env::remove_var(TOKEN_ENV);
    value
}

/// token を含みうる文字列をログ用に潰す。real transport のエラー文字列などに使う。
/// Discord bot token は `.` 区切りの 3 部（`id.timestamp.hmac`）で、先頭は base64 の user id。
/// 保守的に「長い英数字 `.` 英数字 `.` 英数字」列を伏せる。
pub fn redact_token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split_inclusive(|c: char| c.is_whitespace()) {
        let trimmed = word.trim_end();
        if looks_like_token(trimmed) {
            let ws = &word[trimmed.len()..];
            out.push_str("<redacted-token>");
            out.push_str(ws);
        } else {
            out.push_str(word);
        }
    }
    out
}

fn looks_like_token(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            p.len() >= 6
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_removes_from_process_env() {
        std::env::set_var(TOKEN_ENV, "MT2.abcdef.ghijkl");
        let got = take_bot_token();
        assert_eq!(got.as_deref(), Some("MT2.abcdef.ghijkl"));
        assert!(std::env::var(TOKEN_ENV).is_err());
    }

    #[test]
    fn empty_is_none_and_still_removed() {
        std::env::set_var(TOKEN_ENV, "");
        assert_eq!(take_bot_token(), None);
        assert!(std::env::var(TOKEN_ENV).is_err());
    }

    #[test]
    fn redact_blanks_token_shaped_words() {
        let s = "auth failed for MTAx23456.Gabcde.hIJKlmnop-_ retry";
        let r = redact_token(s);
        assert!(!r.contains("MTAx23456.Gabcde.hIJKlmnop-_"), "{r}");
        assert!(r.contains("<redacted-token>"), "{r}");
        assert!(r.contains("auth failed for") && r.contains("retry"), "{r}");
    }

    #[test]
    fn redact_keeps_normal_text() {
        let s = "channel 100.200.300 not a token because short";
        // "100.200.300" parts are too short (< 6) so not redacted.
        assert_eq!(redact_token(s), s);
    }
}
