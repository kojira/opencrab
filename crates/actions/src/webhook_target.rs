//! 通知先（webhook）の設定・解決・秘匿・検証（gateway 非依存）。
//!
//! `crates/discord/src/gateway_actions/webhook.rs` から **純関数群だけ**を降ろしたもの
//! （#157 S4）。ここには
//!
//! - 通知先の設定型（[`WebhookConfig`]）とその出所（[`WebhookSource`]）・解決結果
//!   （[`WebhookResolution`]）
//! - 優先順位付きの解決（[`resolve_subtask_webhook`] / [`resolve_activity_webhook`] /
//!   [`has_activity_default`]）
//! - URL 検証（[`validate_webhook_url`]）と秘匿化（[`redact_webhook_url`] /
//!   [`redact_secrets`]）
//! - 送信先の文字数上限で分ける処理（[`chunk_text`] / [`build_part_messages`]）
//! - 長文を「出だしプレビュー + 全文ファイル添付」1 通に畳むポリシー
//!   （[`build_message_with_optional_attachment`] / [`WebhookMessage`] / #293）
//! - 配送失敗の記録（[`record_webhook_delivery_failure`]。raw url は受け取らない）
//!
//! だけが入る。**実際の HTTP 送信（transport）と Discord 固有の整形は含めない**
//! （それらは discord crate 側に残す）。
//!
//! 依存は `serde_json` / `rusqlite` / `opencrab_db` のみで、gateway crate には依存しない。

use serde_json::json;

include!("webhook_target/message.rs");
include!("webhook_target/resolution.rs");

#[cfg(test)]
mod tests;
