pub mod channel;
pub mod discovery;
pub mod render;
pub mod router;
pub mod session;
pub mod types;

pub use channel::ChatChannel;
pub use router::{AgentHandle, EventBus, MessageRouter, SessionRegistry, new_registry};
pub use session::SessionManager;
pub use types::{DiscoveryEvent, IncomingMessage, MessageSource, SessionEvent, SessionInfo};
