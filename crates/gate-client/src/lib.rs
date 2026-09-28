//! in-tree Rust gateway 共用の V3 client / wire / json。core crate の wire DTO は依存しない。

pub mod client;
pub mod json;
pub mod provision;
pub mod wire;

pub use client::{InvokeHandler, InvokeOutcome, SayPolicy};
pub use provision::{ProvisionClient, ProvisionDesired, Provisioned, ProvisionedBinding};
pub use wire::{FinalDelivery, RuntimeCapabilities};
