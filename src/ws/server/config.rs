use std::net::SocketAddr;
use std::time::Duration;

use crate::client::WsConnectConfig;

use super::types::ManagedWsServerStartError;

/// Static configuration for a managed WebSocket server instance.
///
/// Construct with [`WsServerConfig::new`] and override fields as needed; the
/// defaults are the values documented for the operator dashboard deployment.
/// The struct is intentionally plain data so callers state every transport
/// bound explicitly rather than inheriting an implicit policy.
#[derive(Clone, Copy, Debug)]
pub struct WsServerConfig {
    /// Address the listener binds to. `SO_REUSEADDR` is always set before bind
    /// so a restart can reclaim an address left in `TIME_WAIT`.
    pub bind_addr: SocketAddr,
    /// Listen backlog handed to the OS.
    pub backlog: u32,
    /// Total concurrent connections for this server instance. This is a whole
    /// server budget, **not** a per-IP limit.
    pub max_connections: usize,
    /// Upper bound on the WebSocket handshake for a single accepted socket.
    pub handshake_timeout: Duration,
    /// Cadence of [`WsServerEndpointEvent::Tick`](super::types::WsServerEndpointEvent::Tick)
    /// delivered to every live endpoint handler.
    pub endpoint_tick_interval: Duration,
    /// Budget for draining the listener and all client connections during a
    /// graceful shutdown before connections are aborted.
    pub shutdown_timeout: Duration,
    /// Managed commands retained per client. A client that falls further behind
    /// than this is closed and must reconnect for an authoritative snapshot;
    /// stale frames are never queued on its behalf.
    pub retained_commands_per_client: usize,
    /// WebSocket message/frame limits and write buffering.
    pub websocket: WsConnectConfig,
}

impl WsServerConfig {
    /// Defaults: 256 total connections, backlog 1024, five-second handshake,
    /// one-second endpoint tick, five-second graceful shutdown, one retained
    /// command per client.
    pub fn new(bind_addr: SocketAddr) -> Self {
        Self {
            bind_addr,
            backlog: 1024,
            max_connections: 256,
            handshake_timeout: Duration::from_secs(5),
            endpoint_tick_interval: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(5),
            retained_commands_per_client: 1,
            websocket: WsConnectConfig::default(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ManagedWsServerStartError> {
        if self.backlog == 0 {
            return Err(ManagedWsServerStartError::InvalidConfig {
                field: "backlog",
                reason: "listen backlog must be greater than zero",
            });
        }
        if self.max_connections == 0 {
            return Err(ManagedWsServerStartError::InvalidConfig {
                field: "max_connections",
                reason: "connection cap must admit at least one client",
            });
        }
        if self.handshake_timeout.is_zero() {
            return Err(ManagedWsServerStartError::InvalidConfig {
                field: "handshake_timeout",
                reason: "handshake timeout must be greater than zero",
            });
        }
        if self.endpoint_tick_interval.is_zero() {
            return Err(ManagedWsServerStartError::InvalidConfig {
                field: "endpoint_tick_interval",
                reason: "endpoint tick interval must be greater than zero",
            });
        }
        if self.shutdown_timeout.is_zero() {
            return Err(ManagedWsServerStartError::InvalidConfig {
                field: "shutdown_timeout",
                reason: "shutdown timeout must be greater than zero",
            });
        }
        if self.retained_commands_per_client == 0 {
            return Err(ManagedWsServerStartError::InvalidConfig {
                field: "retained_commands_per_client",
                reason: "at least one managed command must be retained per client",
            });
        }
        Ok(())
    }
}

/// Bind/rebind retry policy for the accept loop.
///
/// Backoff starts at `initial_backoff`, doubles per consecutive failure, and
/// saturates at `max_backoff`. `max_attempts` bounds *consecutive* failures;
/// a successful bind resets the counter. `None` retries forever, which is the
/// self-healing behaviour a non-critical listener wants.
#[derive(Clone, Copy, Debug)]
pub struct WsServerRetryPolicy {
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub max_attempts: Option<u32>,
}

impl Default for WsServerRetryPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(5),
            max_attempts: None,
        }
    }
}

impl WsServerRetryPolicy {
    pub(crate) fn validate(&self) -> Result<(), ManagedWsServerStartError> {
        if self.initial_backoff.is_zero() {
            return Err(ManagedWsServerStartError::InvalidConfig {
                field: "initial_backoff",
                reason: "initial bind backoff must be greater than zero",
            });
        }
        if self.max_backoff < self.initial_backoff {
            return Err(ManagedWsServerStartError::InvalidConfig {
                field: "max_backoff",
                reason: "maximum bind backoff must not be below the initial backoff",
            });
        }
        if self.max_attempts == Some(0) {
            return Err(ManagedWsServerStartError::InvalidConfig {
                field: "max_attempts",
                reason: "retry budget must allow at least one attempt",
            });
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn next_backoff(&self, current: Duration) -> Duration {
        current.saturating_mul(2).min(self.max_backoff)
    }

    /// True once `attempts` consecutive failures have exhausted the budget.
    #[inline]
    pub(crate) fn is_exhausted(&self, attempts: u32) -> bool {
        match self.max_attempts {
            Some(max) => attempts >= max,
            None => false,
        }
    }
}
