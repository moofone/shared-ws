// Canonical definitions live here, but this module is intentionally crate-private
// (see `src/lib.rs`). External callers should use `crate::ws::*`.
pub(crate) mod circular_buffer;
pub(crate) mod frame;
pub(crate) mod global_rate_limit;
pub(crate) mod health;
pub(crate) mod ping;
pub(crate) mod rate_limit;
pub(crate) mod reconnect;
pub(crate) mod types;

// Convenience re-exports for internal modules (`crate::core::{WsFrame, WebSocketError, ...}`).
pub(crate) use frame::*;
pub(crate) use types::*;
