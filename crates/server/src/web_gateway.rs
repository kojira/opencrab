//! web gateway の再エクスポートシム（実体は `crates/web-gateway` / #190 S3）。
//!
//! ゲートウェイの実体（SSE 配信チャンネル、セッション ID 規約、応答生成の入口、
//! subtask 完了の受け口）は独立クレート `opencrab-web-gateway` へ移した。#191 の方針
//! （コアを生かしたまま外側を差し替えられる構成）に沿って、web の実装が
//! `crates/server` の型（`AppState` / `process` / `transcript`）に触らないようにするため。
//!
//! server 側は [`WebAgentRunner`] を `AppState` に実装して繋ぐ
//! （`crate::web_runner_impl`）。このモジュールは既存の参照
//! （`crate::web_gateway::WebGateway` など。`AppState` のフィールド型名を含む）を
//! 壊さないための薄い再エクスポートだけを持つ。HTTP ハンドラ（`crate::api::web`）の
//! 移動は別段（S4）。

pub use opencrab_web_gateway::{
    caller_type_label, run_and_deliver_serialized, web_session_id, WebAgentRunner,
    WebCompletionSink, WebEvent, WebGateway, WEB_SESSION_PREFIX, WEB_SESSION_THEME,
};
