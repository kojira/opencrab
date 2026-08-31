//! QC ハーネス用の差し替えフラグ。**production 経路はフラグ OFF で従来どおり**（Nostr の
//! `HarnessOverrides` と同形）。トークン・ネットワーク不要の決定的オフライン E2E を組む 2 点:
//!
//! - `fake_events`: serenity Gateway 接続の代わりに、指定 JSONL fixture（Discord Message Create
//!   相当）を流して said へ写す（[`crate::receive::run_fake_events_once`]）。
//! - `dry_run`: say / reply / reaction / resolve を Discord REST へ出さず、種別・対象・本文を INFO
//!   ログに残して core へは成功を返す（[`crate::transport::DryRunTransport`]）。
//!
//! いずれも既定は無効。env から読むのは実バイナリの `main` だけ（[`HarnessOverrides::from_env`]）。
//! テストは env を触らず `HarnessOverrides` を直接構築するので、並列実行でも env 競合しない。

use std::path::PathBuf;

/// 偽イベント fixture パスを指す env 変数。値が空なら無効。
pub const FAKE_EVENTS_ENV: &str = "OPENCRAB_DISCORDGATE_FAKE_EVENTS";
/// 送信 dry-run を有効にする env 変数（`1` / `true`）。
pub const DRY_RUN_ENV: &str = "OPENCRAB_DISCORDGATE_DRY_RUN";

/// QC ハーネスの差し替え設定。既定（`Default`）は全 OFF＝production 挙動。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HarnessOverrides {
    /// Some のとき serenity Gateway の代わりにこの JSONL fixture を流す。
    pub fake_events: Option<PathBuf>,
    /// true のとき say/reply/reaction/resolve を REST へ出さずログに残して成功を返す。
    pub dry_run: bool,
}

impl HarnessOverrides {
    /// env からハーネス設定を読む。**実バイナリの起動時に 1 回だけ**呼ぶ想定。
    pub fn from_env() -> Self {
        let fake_events = std::env::var(FAKE_EVENTS_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let dry_run = std::env::var(DRY_RUN_ENV)
            .ok()
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Self {
            fake_events,
            dry_run,
        }
    }

    /// いずれかの差し替えが有効か（起動ログで production でない旨を警告するため）。
    pub fn is_active(&self) -> bool {
        self.fake_events.is_some() || self.dry_run
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_off() {
        let d = HarnessOverrides::default();
        assert!(d.fake_events.is_none());
        assert!(!d.dry_run);
        assert!(!d.is_active(), "既定は production 挙動（差し替え無し）");
    }

    #[test]
    fn is_active_when_any_override_set() {
        assert!(HarnessOverrides {
            fake_events: Some(PathBuf::from("/x/fix.jsonl")),
            dry_run: false,
        }
        .is_active());
        assert!(HarnessOverrides {
            fake_events: None,
            dry_run: true,
        }
        .is_active());
    }
}
