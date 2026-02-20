pub mod backend;
pub mod discovery;
pub mod render;
pub mod session;
pub mod types;

pub use backend::ChatBackend;
pub use session::SessionManager;
pub use types::{DiscoveryEvent, IncomingMessage, MessageSource, SessionEvent, SessionInfo};
