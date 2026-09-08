//! watch 用 nsec。process env から除去し、child env にだけ渡す。

pub const SECRET_ENV: &str = "NOSTARO_SECRET_KEY";

/// 起動時に 1 回読む。直後に process env から消す。空は「鍵なし」。
pub fn take_watch_secret() -> Option<String> {
    let value = std::env::var(SECRET_ENV).ok().filter(|s| !s.is_empty());
    std::env::remove_var(SECRET_ENV);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_removes_from_process_env() {
        // env を書き換えるので、子 spawn テスト（post/watch）と直列化する（#868）。
        let _env = crate::ENV_LOCK.blocking_lock();
        std::env::set_var(SECRET_ENV, "nsec1testsecret");
        let got = take_watch_secret();
        assert_eq!(got.as_deref(), Some("nsec1testsecret"));
        assert!(std::env::var(SECRET_ENV).is_err());
        drop(got);
    }
}
