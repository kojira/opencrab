//! OpenCrab の web ゲートウェイ（ダッシュボード会話 / #154・切り出しは #190 S3・S4）。
//!
//! ダッシュボードからエージェントと会話するためのゲートウェイ。HTTP 境界（axum の
//! ルータとハンドラ / [`http`]）と**ゲートウェイの実体**（SSE 配信チャンネル、
//! セッション ID 規約、応答生成の入口、subtask 完了の受け口）の両方がここに入る。
//! 上位（`crates/server` の `create_router`）は [`http::routes`] を `.merge()` で
//! 取り付けるだけでよい。
//!
//! `crates/server` には依存しない。エージェント実行と永続化に必要な操作は
//! [`WebAgentRunner`] トレイト越しに呼ぶ（`crates/server` の `AppState` が実装する）。
//! トレイトには DB 行の型を出さない — 会話の保存や認可判定の**結果**だけを受け取り、
//! スキーマの変更がゲートウェイ層へ波及しないようにする。
//!
//! ## モジュール構成が守っている不変条件（#177）
//!
//! 同一セッションでは「inbound への応答」と「subtask 完了 resume の応答」を直列化
//! しなければならない（並行すると同じ履歴から 2 通の応答が出る = 二重回答）。
//! これを**コンパイル時に**強制するため、
//!
//! - 生の応答生成 [`respond`] 内の `run_and_deliver` は **module-private**、
//! - 外へ出す入口は直列化込みの [`run_and_deliver_serialized`] だけ、
//! - 完了受け口 [`sink`] は**別モジュール**（兄弟）。
//!
//! Rust では兄弟モジュールから private 項目へ到達できないため、sink が生の応答生成を
//! 直呼びするコードはコンパイルできない（同一モジュール内 private だと直呼びできて
//! しまい、直列化の呼び忘れが型で防げない）。HTTP ハンドラ（[`http`]）も同じく兄弟
//! モジュールなので、公開入口以外を呼べない。
//!
//! ## Discord / Nostr との対比
//!
//! - Discord は完了通知を `LoopEvent`（serenity 依存のイベントループ）へ送る。
//! - web は `LoopEvent` を持たない。[`WebCompletionSink`] が直接 `tokio::spawn` して
//!   per-session ロック下で resume し、SSE で配送する。Discord 固有型は持ち込まない。
//!
//! 不変条件（RFC #152 §6）:
//! - **二重回答**: `settle_completed` が「DB 永続化 → sink 発火」の順序を保証済み。
//!   resume は会話履歴を DB から再構築する（[`WebAgentRunner::build_conversation_string`]）。
//! - **per-session 直列化**: [`SessionRuntime`](opencrab_actions::SessionRuntime) の
//!   1 本のロックで inbound / resume を直列化する。異なるセッションは並行。

pub mod gateway;
pub mod http;
pub mod respond;
pub mod runner;
pub mod sink;

#[cfg(test)]
mod testing;

pub use gateway::{
    caller_type_label, web_session_id, WebEvent, WebGateway, WEB_SESSION_PREFIX, WEB_SESSION_THEME,
};
pub use http::routes;
pub use respond::run_and_deliver_serialized;
pub use runner::WebAgentRunner;
pub use sink::WebCompletionSink;
