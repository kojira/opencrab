//! Nostr gateway。watch JSONL → V3 said（inbound）／ V3 say → nostaro reply（outbound）。

pub mod config;
pub mod dedup;
pub mod map;
pub mod ops;
pub mod post;
pub mod run;
pub mod secret;
pub mod watch;
