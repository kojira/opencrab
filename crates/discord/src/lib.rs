//! Discord webhook notification adapters.
//!
//! Discord ingress and delivery run exclusively in the external `discord-gateway` process.

mod gateway_actions;

pub use gateway_actions::{spawn_activity_tool_event_sink, DiscordWebhookNotifier};
