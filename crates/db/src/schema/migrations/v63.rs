//! v63: drop the platform settings tables left in core by the gateway separation (D-1097).
//!
//! Discord and Nostr settings (bot tokens, Nostr keys, owners, trusted users, co-agents,
//! channels, relay webhooks, watches, voice) are owned and stored by the gateways. Nothing in
//! core reads these tables any more, and some hold a plaintext bot token or a Nostr secret key.

use super::Migration;

/// Platform settings tables removed from core. Kept in sync with the fresh-schema cleanup in
/// `schema::initialize`.
pub(in crate::schema) const LEGACY_GATEWAY_SETTINGS_TABLES: [&str; 10] = [
    "trusted_users",
    "channel_config",
    "session_watches",
    "agent_discord_config",
    "agent_nostr_config",
    "agent_nostr_relay_config",
    "discord_channel_config",
    "trusted_co_agents",
    "voice_config_override",
    "nostr_bundle_state",
];

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 63,
    description: "drop legacy gateway settings tables (D-1097)",
    up: |conn| {
        for table in LEGACY_GATEWAY_SETTINGS_TABLES {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
        }
        Ok(())
    },
}];
