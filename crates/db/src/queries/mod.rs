//! DB クエリ層（#34: 5900行の単一モジュールをドメイン別に分割）。
//!
//! 各ドメインの行型とクエリはサブモジュールが所有する。全公開アイテムは
//! ここから再輸出されるため、消費側の `opencrab_db::queries::X` パスは
//! 分割前と変わらない。新しいドメインを足すときは新しいサブモジュールに。

mod agent_discord_config;
mod agent_logs;
mod agent_nostr_config;
mod agents;
mod allowed_commands;
mod channel_config;
mod curated_memory;
mod heartbeat;
mod impressions;
mod llm_logs;
mod llm_metrics;
mod memory_index;
mod model_pricing;
mod pending_interactions;
mod provider_settings;
mod session_logs;
mod sessions;
mod skills;
mod sync_state;
mod task_ledger;
mod trusted_users;
mod webhook_config;

pub use agent_discord_config::*;
pub use agent_logs::*;
pub use agent_nostr_config::*;
pub use agents::*;
pub use allowed_commands::*;
pub use channel_config::*;
pub use curated_memory::*;
pub use heartbeat::*;
pub use impressions::*;
pub use llm_logs::*;
pub use llm_metrics::*;
pub use memory_index::*;
pub use model_pricing::*;
pub use pending_interactions::*;
pub use provider_settings::*;
pub use session_logs::*;
pub use sessions::*;
pub use skills::*;
pub use sync_state::*;
pub use task_ledger::*;
pub use trusted_users::*;
pub use webhook_config::*;

#[cfg(test)]
mod tests;
