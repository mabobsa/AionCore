#![warn(clippy::disallowed_types)]

//! Cross-session message delivery.
//!
//! Delivery delegates to `ConversationService::send_message` — the human send
//! path — to share transport behavior rather than keep two paths in sync.
//! The recipient block identifies an inter-agent report or request; reusing the
//! transport does not grant user authority or new user approval to its content.

pub mod drainer;
pub mod error;
pub mod hook;
pub mod queue;
pub mod rate_limit;
// Fork: cross-machine delivery route.
pub mod remote_routes;
pub mod routes;
pub mod service;
pub mod state;
pub mod targets;

pub use error::SessionMessageError;
pub use hook::QueueClearingCancelHook;
pub use remote_routes::session_message_remote_routes;
pub use routes::{session_message_routes, session_message_user_routes};
pub use service::{
    RemoteSessionDelivery, RemoteSessionRelay, RemoteSessionRelayRequest, SessionMessageDeps, SessionMessageService,
};
pub use state::SessionMessageRouterState;
pub use targets::MentionableTargets;
