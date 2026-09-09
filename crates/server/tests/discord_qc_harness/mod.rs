//! Shared wiring and scenario-contract modules for the Discord offline E2E harness.

mod support;

mod base_delivery;
mod chunking;
mod completion_continue;
mod completion_edges;
mod completion_lifecycle;
mod folding;
mod heartbeat;
mod holding_resume;
mod no_reply;
mod read_reactions;
