//! Runtime-agnostic WebSocket infrastructure.

pub mod client;
// Internal canonical definitions. Public API is exposed via `crate::ws` (and select root re-exports).
mod core;
pub mod testing;
pub mod tls;
pub mod transport;
pub mod ws;
