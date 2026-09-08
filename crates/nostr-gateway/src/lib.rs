//! Nostr gateway。watch JSONL → V3 said（inbound）／ V3 say → nostaro reply（outbound）。

pub mod config;
pub mod dedup;
pub mod harness;
pub mod map;
pub mod ops;
pub mod post;
pub mod run;
pub mod secret;
pub mod watch;

/// テスト専用: プロセス env を触るテストと、子プロセスを spawn するテストを直列化する。
///
/// `std::env::{set_var,remove_var}` は environ を無同期で書き換える。別スレッドが
/// `fork`+`exec`（子 spawn は environ を読んで child env を組む）を実行中に environ を
/// 書き換えると、まれに spawn/exec が壊れて子が失敗する。#868 の flaky
/// （`post::tests::say_invokes_nostaro_post_with_env_secret` が `assert_eq!(got,
/// PostedStandalone)` で稀に落ちる）はこの競合。writer は `secret` の env 除去テスト、
/// reader は `post`/`watch` の子 spawn テスト。同一ロックで排他して決定化する
/// （`crates/server/src/config.rs` の `env_lock` と同じ流儀）。
///
/// sync テスト（`secret`）と async テスト（`post`/`watch`）が同じロックを共有するため
/// `tokio::sync::Mutex` を使う（sync 側は `blocking_lock`・async 側は `lock().await`）。
/// これにより `clippy::await_holding_lock`（std Mutex を await 跨ぎで保持）も避けられる。
#[cfg(test)]
pub(crate) static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
