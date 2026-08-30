//! QC ハーネス用の差し替えフラグ。**production 経路はフラグ OFF で従来どおり**。
//!
//! リレー・鍵不要の決定的オフライン E2E を組むための 2 点:
//! - `fake_watch`: `nostaro watch` の spawn を、指定 JSONL fixture を流して保持する
//!   「偽 watch」へ差し替える（[`crate::watch::run_fake_watch_once`]）。
//! - `dry_run`: say/投稿を publish せず本文・種別を INFO ログに残し、core へは成功 ack
//!   を返す（[`crate::post::deliver_say`]）。
//!
//! いずれも既定は無効。env から読むのは実バイナリの `main` だけ（[`HarnessOverrides::from_env`]）。
//! テストは env を触らず `HarnessOverrides` を直接構築するので、並列実行でも env 競合しない。

use std::path::PathBuf;

/// 偽 watch fixture パスを指す env 変数。値が空なら無効。
pub const FAKE_WATCH_ENV: &str = "OPENCRAB_NOSTRGATE_FAKE_WATCH";
/// 投稿 dry-run を有効にする env 変数（`1` / `true`）。
pub const DRY_RUN_ENV: &str = "OPENCRAB_NOSTRGATE_DRY_RUN";

/// QC ハーネスの差し替え設定。既定（`Default`）は全 OFF＝production 挙動。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HarnessOverrides {
    /// Some のとき `nostaro watch` の代わりにこの JSONL fixture を流す偽 watch を使う。
    pub fake_watch: Option<PathBuf>,
    /// true のとき say を publish せずログに残して成功 ack を返す。
    pub dry_run: bool,
}

impl HarnessOverrides {
    /// env からハーネス設定を読む。**実バイナリの起動時に 1 回だけ**呼ぶ想定。
    pub fn from_env() -> Self {
        let fake_watch = std::env::var(FAKE_WATCH_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let dry_run = std::env::var(DRY_RUN_ENV)
            .ok()
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Self {
            fake_watch,
            dry_run,
        }
    }

    /// いずれかの差し替えが有効か（起動ログで production でない旨を警告するため）。
    pub fn is_active(&self) -> bool {
        self.fake_watch.is_some() || self.dry_run
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_off() {
        let d = HarnessOverrides::default();
        assert!(d.fake_watch.is_none());
        assert!(!d.dry_run);
        assert!(!d.is_active(), "既定は production 挙動（差し替え無し）");
    }

    #[test]
    fn is_active_when_any_override_set() {
        assert!(HarnessOverrides {
            fake_watch: Some(PathBuf::from("/x/fix.jsonl")),
            dry_run: false,
        }
        .is_active());
        assert!(HarnessOverrides {
            fake_watch: None,
            dry_run: true,
        }
        .is_active());
    }
}
