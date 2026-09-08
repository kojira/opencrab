//! V3 protocol=2 の独立実装。core crate の wire DTO は依存しない。
//! client / wire / json は `opencrab-gate-client` を再 export する。

pub use opencrab_gate_client::{client, json, wire};

pub mod config;
pub mod http;
