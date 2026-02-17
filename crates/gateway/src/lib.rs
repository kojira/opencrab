pub mod traits;
pub mod message;
pub mod adapters;

pub use traits::Gateway;
pub use message::{
    IncomingMessage, OutgoingMessage,
    MessageSource, MessageContent, ContentPart,
    MessageTarget, Sender, Channel,
    GatewayConfig,
};
pub use adapters::rest::RestGateway;
pub use adapters::cli::CliGateway;
pub use adapters::websocket::WebSocketGateway;
pub use adapters::discord::DiscordGateway;
