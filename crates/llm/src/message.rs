//! LLM message model.
//!
//! The canonical types now live in the leaf crate `opencrab-llm-types` so that
//! `opencrab-core` (the engine) and `opencrab-llm` (providers/router) can share
//! one model. This module re-exports them unchanged, so existing
//! `crate::message::*` / `opencrab_llm::message::*` imports keep working.
pub use opencrab_llm_types::*;
