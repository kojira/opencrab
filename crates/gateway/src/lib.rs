pub mod adapters;
pub mod message;
pub mod traits;

pub use message::{Channel, ContentPart, IncomingMessage, MessageContent, MessageSource, Sender};
pub use traits::{
    GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext, GatewayCaller,
    PEER_REVIEW_REPLY_MARKER, PEER_REVIEW_REQUEST_MARKER,
};

#[cfg(feature = "discord")]
pub use adapters::discord::DiscordGateway;

#[cfg(feature = "discord")]
pub use adapters::discord::{
    A2uiFormModalResolver, A2uiFormModalSpec, ComponentInteractionData, InteractionKind,
};
