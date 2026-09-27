//! Web 会話の独立 gateway。HTTP/SSE ⇄ V3 変換のみ。判断しない。Bearer を持たない。

pub mod admin;
pub mod owner;
pub mod secret_store;
pub mod store;
pub mod v3;
