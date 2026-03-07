//! Runtime-agnostic WebSocket infrastructure.

pub mod client;
// Internal canonical definitions. Public API is exposed via `crate::ws` (and select root re-exports).
mod core;
pub mod fixture_policy;
pub mod testing;
pub mod tls;
pub mod transport;
pub mod ws;

pub use fixture_policy::{
    WsFixtureRequirement, clear_required_ws_contracts_for_tests, ensure_live_ws_allowed,
    fixture_capture_mode_enabled as ws_fixture_capture_mode_enabled,
    register_required_ws_contracts, required_ws_contracts,
};
