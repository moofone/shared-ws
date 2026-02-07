use std::fmt::Debug;
use std::marker::PhantomData;
use std::time::Duration;

use bytes::Bytes;
use sonic_rs::Value;
use thiserror::Error;

use super::frame::WsFrame;

/// Convenience result alias for websocket operations.
pub type WebSocketResult<T> = Result<T, WebSocketError>;

/// Ingress decision produced by a tight-loop decoder running outside the actor runtime.
///
/// This allows providers to parse/aggregate/filter in the IO loop, only forwarding the
/// minimal set of "interesting" events into the actor mailbox.
#[derive(Debug)]
pub enum WsIngressAction<M>
where
    M: Send + 'static,
{
    /// Ignore this frame (still counts as inbound activity for stale detection).
    Ignore,
    /// Emit an application event into the actor.
    Emit(M),
    /// Forward the raw frame to the actor for full handling.
    Forward(WsFrame),
    /// Request reconnect with a reason.
    Reconnect(String),
    /// Request full shutdown with a reason.
    Shutdown(String),
}

/// Tight-loop decoder/aggregator/filter that runs outside kameo (in the reader task).
///
/// The actor owns this state and moves it into the reader task on connect; on disconnect
/// the state is handed back to the actor to preserve continuity across reconnects.
pub trait WsIngress: Send + 'static {
    type Message: Send + 'static;

    fn on_open(&mut self) {}

    fn on_frame(&mut self, frame: WsFrame) -> WsIngressAction<Self::Message>;

    fn on_disconnect(&mut self, _cause: &WsDisconnectCause) {}
}

/// Default ingress that forwards every frame into the actor.
#[derive(Debug, Clone, Copy)]
pub struct ForwardAllIngress<M = ()>(PhantomData<fn() -> M>);

impl<M> Default for ForwardAllIngress<M> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<M> WsIngress for ForwardAllIngress<M>
where
    M: Send + 'static,
{
    type Message = M;

    #[inline]
    fn on_frame(&mut self, frame: WsFrame) -> WsIngressAction<Self::Message> {
        WsIngressAction::Forward(frame)
    }
}

/// Canonical websocket error surface shared across the infrastructure.
#[derive(Debug, Error)]
pub enum WebSocketError {
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Authentication failed: {message}")]
    AuthenticationFailed { message: String, code: Option<i32> },

    #[error("Subscription failed: {message}, channel={channel}")]
    SubscriptionFailed { message: String, channel: String },

    #[error("Transport error ({context}): {error}")]
    TransportError {
        context: &'static str,
        error: String,
    },

    #[error("Parse failed: {0}")]
    ParseFailed(String),

    #[error("Actor error: {0}")]
    ActorError(String),

    #[error("Timeout: {context}")]
    Timeout { context: String },

    #[error("Stale connection: no data for {seconds}s")]
    StaleConnection { seconds: u64 },

    #[error("Server error: code={code:?}, message={message}")]
    ServerError { code: Option<i32>, message: String },

    #[error("Invalid state: {0}")]
    InvalidState(String),

    #[error("Endpoint error: {0}")]
    EndpointSpecific(String),

    #[error("Backpressure: outbound queue full")]
    OutboundQueueFull,

    #[error("Rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after: Option<Duration>,
    },
}

/// Transport-independent buffer sizing parameters used for websocket configuration.
#[derive(Clone, Copy, Debug)]
pub struct WebSocketBufferConfig {
    pub read_buffer_bytes: usize,
    pub write_buffer_bytes: usize,
    pub max_write_buffer_bytes: usize,
    pub max_message_bytes: usize,
    pub max_frame_bytes: usize,
}

impl Default for WebSocketBufferConfig {
    fn default() -> Self {
        Self {
            // Read buffer sized to comfortably hold typical provider frames without growth.
            read_buffer_bytes: 16 * 1024 * 1024,
            write_buffer_bytes: 128 << 10,
            max_write_buffer_bytes: 256 << 10,
            max_message_bytes: 16 * 1024 * 1024,
            max_frame_bytes: 16 * 1024 * 1024,
        }
    }
}

/// TLS configuration for websocket connections.
///
/// Safe-by-default: certificate validation is enabled unless explicitly disabled for development /
/// controlled environments.
#[derive(Clone, Copy, Debug)]
pub struct WsTlsConfig {
    pub validate_certs: bool,
}

impl Default for WsTlsConfig {
    fn default() -> Self {
        Self {
            validate_certs: true,
        }
    }
}

/// Latency policy percentile selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsLatencyPercentile {
    P50,
    P99,
}

impl WsLatencyPercentile {
    pub fn as_f64(self) -> f64 {
        match self {
            WsLatencyPercentile::P50 => 50.0,
            WsLatencyPercentile::P99 => 99.0,
        }
    }
}

/// Runtime policy describing how the latency monitor should behave.
#[derive(Clone, Copy, Debug)]
pub struct WsLatencyPolicy {
    pub percentile: WsLatencyPercentile,
    pub threshold: Duration,
    pub min_samples: u64,
    pub consecutive_breaches: u32,
}

/// Registration options for distributed actor discovery.
#[derive(Clone, Debug)]
pub struct WsActorRegistration {
    pub name: String,
}

impl WsActorRegistration {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

/// Basic connection statistics snapshot.
#[derive(Clone, Debug)]
pub struct WsConnectionStats {
    pub uptime: Duration,
    pub messages: u64,
    pub errors: u64,
    pub reconnects: u64,
    pub last_message_age: Duration,
    pub recent_server_errors: usize,
    pub recent_internal_errors: usize,
    pub p50_latency_us: u64,
    pub p99_latency_us: u64,
    pub latency_samples: u64,
}

/// Canonical disconnect causes enumerated by the infrastructure.
#[derive(Debug, Clone)]
pub enum WsDisconnectCause {
    StaleData,
    PongTimeout,
    RemoteClosed,
    ReadFailure {
        error: String,
    },
    EndpointRequested {
        reason: String,
    },
    ServerError {
        code: Option<i32>,
        message: String,
    },
    InternalError {
        context: String,
        error: String,
    },
    HandshakeFailed {
        status: Option<u16>,
        message: String,
    },
    HighLatency {
        percentile: f64,
        rtt_us: u64,
    },
}

/// Actions the reconnect policy may take after a disconnect classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsDisconnectAction {
    ImmediateReconnect,
    BackoffReconnect,
    Abort,
}

/// High-level websocket connection status surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsConnectionStatus {
    Connecting,
    Connected,
    Disconnected,
}

/// Message queue actions emitted by the parse pipeline.
#[derive(Debug, Clone)]
pub enum WsMessageAction<T> {
    Continue,
    Process(T),
    Reconnect(String),
    Shutdown,
}

/// Outcomes of decoding a websocket frame.
#[derive(Debug, Clone)]
pub enum WsParseOutcome<T> {
    ServerError {
        code: Option<i32>,
        message: String,
        data: Option<Value>,
    },
    Message(WsMessageAction<T>),
}

/// Subscription management primitives shared by websocket clients.
#[derive(Debug, Clone)]
pub enum WsSubscriptionAction<T> {
    Add(Vec<T>),
    Remove(Vec<T>),
    Replace(Vec<T>),
    Clear,
}

/// Result returned when a subscription response is processed.
#[derive(Debug, Clone)]
pub enum WsSubscriptionStatus {
    Acknowledged {
        success: bool,
        message: Option<String>,
    },
    NotSubscriptionResponse,
}

/// Trait describing a subscription lifecycle for a websocket endpoint.
pub trait WsSubscriptionManager: Send + Sync + 'static {
    type SubscriptionMessage: Send + 'static;

    fn initial_subscriptions(&mut self) -> Vec<Self::SubscriptionMessage>;

    fn update_subscriptions(
        &mut self,
        action: WsSubscriptionAction<Self::SubscriptionMessage>,
    ) -> Result<Vec<Self::SubscriptionMessage>, String>;

    fn serialize_subscription(&mut self, msg: &Self::SubscriptionMessage) -> Vec<u8>;

    fn handle_subscription_response(&mut self, data: &[u8]) -> WsSubscriptionStatus;
}

/// Application-specific websocket handler interface.
pub trait WsEndpointHandler: Send + Sync + 'static {
    type Message: Send + 'static;
    type Error: std::error::Error + Send + Sync;
    type Subscription: WsSubscriptionManager;

    fn subscription_manager(&mut self) -> &mut Self::Subscription;

    fn generate_auth(&self) -> Option<Vec<u8>>;

    fn parse(&mut self, data: &[u8]) -> Result<WsParseOutcome<Self::Message>, Self::Error>;

    fn handle_message(&mut self, msg: Self::Message) -> Result<(), Self::Error>;

    fn handle_server_error(
        &mut self,
        code: Option<i32>,
        message: &str,
        data: Option<Value>,
    ) -> WsErrorAction;

    fn reset_state(&mut self);

    fn classify_disconnect(&self, cause: &WsDisconnectCause) -> WsDisconnectAction;

    fn on_open(&mut self) {}

    /// Optional instrumentation hook: emit payload-provided timestamps for distributed lag metrics.
    ///
    /// This is called only when the websocket actor's payload latency sampling is enabled and a
    /// sample is due. Emit timestamps as **Unix epoch microseconds**.
    ///
    /// `kind` should be a stable identifier (e.g. `"event_time"`, `"sent_at"`, `"exchange_ts"`).
    /// The actor will compute `now_us - ts_us` and forward the lag to the configured metrics hook.
    #[inline]
    fn sample_payload_timestamps_us(
        &mut self,
        _data: &[u8],
        _emit: &mut dyn FnMut(&'static str, i64),
    ) {
    }
}

/// Configuration for opt-in payload timestamp lag sampling.
#[derive(Clone, Copy, Debug)]
pub struct WsPayloadLatencySamplingConfig {
    /// Minimum time between samples. Typical values are 5-30s.
    pub interval: Duration,
}

impl WsPayloadLatencySamplingConfig {
    /// Best-effort current time as Unix epoch microseconds.
    #[inline]
    pub fn now_epoch_us() -> Option<i64> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let dur = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
        let us = dur.as_micros().min(i64::MAX as u128) as i64;
        Some(us)
    }
}

/// Recovery actions produced when a server or internal error is encountered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsErrorAction {
    Continue,
    Reconnect,
    Fatal,
}

/// Abstract reconnect strategy trait.
pub trait WsReconnectStrategy: Send + Sync + 'static {
    fn next_delay(&mut self) -> Duration;
    fn reset(&mut self);
    fn should_retry(&self) -> bool;
}

/// Fixed reusable buffers owned by a websocket actor to avoid per-frame allocations.
#[derive(Debug)]
pub struct WsBufferPool {
    inbound: bytes::BytesMut,
    outbound: bytes::BytesMut,
    inbound_capacity: usize,
}

impl WsBufferPool {
    pub fn new(cfg: WebSocketBufferConfig) -> Self {
        let inbound_capacity = cfg.read_buffer_bytes;
        Self {
            inbound: bytes::BytesMut::with_capacity(inbound_capacity),
            outbound: bytes::BytesMut::with_capacity(cfg.write_buffer_bytes),
            inbound_capacity,
        }
    }

    /// Copy payload into the reusable inbound buffer, erroring if it exceeds the fixed capacity.
    pub fn prepare_inbound<'a>(&'a mut self, payload: &[u8]) -> WebSocketResult<&'a [u8]> {
        if payload.len() > self.inbound_capacity {
            return Err(WebSocketError::ParseFailed(format!(
                "frame too large: {} > inbound buffer {}",
                payload.len(),
                self.inbound_capacity
            )));
        }
        self.inbound.clear();
        self.inbound.extend_from_slice(payload);
        Ok(self.inbound.as_ref())
    }

    /// Mutable outbound scratch buffer (reserved for future zero-copy writers).
    pub fn outbound_mut(&mut self) -> &mut bytes::BytesMut {
        &mut self.outbound
    }

    pub fn inbound_capacity(&self) -> usize {
        self.inbound_capacity
    }
}

/// Convenience alias.
pub type WsMessage = WsFrame;

/// Convert owned bytes into a websocket frame while avoiding extra copies.
#[inline]
pub fn into_ws_message<B>(bytes: B) -> WsFrame
where
    B: Into<Bytes>,
{
    super::frame::into_ws_frame(bytes)
}

/// Borrow the underlying bytes from frames without allocation.
#[inline]
pub fn message_bytes(frame: &WsFrame) -> Option<&[u8]> {
    super::frame::frame_bytes(frame)
}
