pub mod actor;
pub mod health;
pub mod ping;
pub mod rate_limit;
pub mod types;
pub mod writer;

use std::sync::Arc;

// Public surface: keep websocket-facing API under `crate::ws::*` and avoid exposing `crate::core`.
pub use crate::core::frame::{WsCloseFrame, WsFrame, frame_bytes, into_ws_frame};
pub use crate::core::global_rate_limit::{
    AcquirePermits, AcquiredPermits, SetProviderLimit, WsGlobalRateLimitHandle,
    WsGlobalRateLimiterActor, WsProviderId, WsRateLimitConfig,
    spawn_global_rate_limiter_supervised_with, spawn_global_rate_limiter_supervisor,
};
pub use crate::core::health::WsHealthMonitor;
pub use crate::core::ping::{
    ProtocolPingPong, WsApplicationPingPong, WsPingPongStrategy, WsPongResult,
};
pub use crate::core::rate_limit::{WsCircuitBreaker, WsRateLimiter, jitter_delay};
pub use crate::core::reconnect::ExponentialBackoffReconnect;
pub use crate::core::types::*;

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
