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

    /// Observe payload lag computed from a payload-provided timestamp:
    /// `lag_us = now_epoch_us - payload_ts_us`.
    ///
    /// This is opt-in and called at a low sampling frequency.
    #[inline]
    fn observe_payload_lag_us(&self, _connection_id: &str, _kind: &'static str, _lag_us: u64) {}
}

/// Convenient alias for passing around boxed metric hooks.
pub type WsMetricsHook = Arc<dyn WsMetricsReporter>;

pub use actor::*;
pub use writer::*;
