pub mod actor;
pub mod health;
pub mod ping;
pub mod rate_limit;
pub mod types;
pub mod writer;

use std::sync::Arc;

pub use crate::core::*;

/// Hook interface for surfacing websocket metrics without coupling to
/// chain-specific observability actors.
pub trait WsMetricsReporter: Send + Sync + 'static {
    fn track_writer_error(&self, connection_id: &str);
}

/// Convenient alias for passing around boxed metric hooks.
pub type WsMetricsHook = Arc<dyn WsMetricsReporter>;

pub use actor::*;
pub use writer::*;
