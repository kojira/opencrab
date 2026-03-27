pub mod adapters;
pub mod message;
pub mod traits;

pub use adapters::cli::CliGateway;
pub use adapters::rest::RestGateway;
pub use adapters::websocket::WebSocketGateway;
pub use message::{
    Channel, ContentPart, GatewayConfig, IncomingMessage, MessageContent, MessageSource,
    MessageTarget, OutgoingMessage, Sender,
};
pub use traits::Gateway;
pub use traits::{GatewayActionDef, GatewayActionResult, GatewayActions};

#[cfg(feature = "discord")]
pub use adapters::discord::DiscordGateway;

#[cfg(feature = "discord")]
pub use adapters::discord::{
    A2uiFormModalResolver, A2uiFormModalSpec, ComponentInteractionData, InteractionKind,
};
