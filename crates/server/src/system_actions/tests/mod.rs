use super::*;
use opencrab_gateway::GatewayCaller;

mod support;
use support::*;

include!("definitions.rs");
include!("agent_heartbeat_basics.rs");
include!("agent_heartbeat_schedule.rs");
include!("allowed_command_listing.rs");
include!("allowed_command_management.rs");
include!("cancel_subtask.rs");
include!("heartbeat_instructions.rs");
include!("management_transport.rs");
include!("memory_index_config.rs");
include!("nostr_run.rs");
include!("peer_review.rs");
include!("send_ui.rs");
include!("skills.rs");
include!("subtask_control.rs");
include!("webhook_targets.rs");
