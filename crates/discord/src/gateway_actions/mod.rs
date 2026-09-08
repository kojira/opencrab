//! Discord webhook notification adapters retained independently of ingress.

mod subtask_engine;
mod subtask_notifier;
mod webhook;

pub use subtask_engine::spawn_activity_tool_event_sink;
pub use subtask_notifier::DiscordWebhookNotifier;
