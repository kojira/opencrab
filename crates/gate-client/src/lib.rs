//! in-tree Rust gateway 共用の V3 client / wire / json。core crate の wire DTO は依存しない。

pub mod admin;
pub mod client;
pub mod emission;
pub mod json;
pub mod wire;

pub use client::{DeliveryOutcome, InvokeHandler, InvokeOutcome, SayPolicy};
pub use wire::{DeliveryGuarantee, FinalDelivery, RuntimeCapabilities};
