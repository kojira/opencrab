//! in-tree Rust gateway 共用の V3 client / wire / json。core crate の wire DTO は依存しない。

pub mod client;
pub mod json;
pub mod wire;

pub use client::SayPolicy;
