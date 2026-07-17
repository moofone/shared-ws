use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use crate::core::frame::{WsCloseFrame, WsFrame};

/// Observable lifecycle state of a managed server.
///
/// `Stopped` and `Terminal` are mutually exclusive end states: a normal
/// shutdown always lands on `Stopped` and can never be reported as a terminal
/// failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ManagedWsServerStatus {
    /// Task spawned, first bind not yet attempted.
    Starting,
    /// Bind failed; backing off before the next attempt. Not a failure state —
    /// the server self-heals from here once the address frees up.
    Retrying,
    /// Listener bound and accepting.
    Listening,
    /// Graceful shutdown in progress; listener closed, clients draining.
    ShuttingDown,
    /// Clean end state.
    Stopped,
    /// Unrecoverable end state. [`ManagedWsServerHandle::terminal_error`] holds
    /// the cause.
    ///
    /// [`ManagedWsServerHandle::terminal_error`]: super::handle::ManagedWsServerHandle::terminal_error
    Terminal,
}

impl ManagedWsServerStatus {
    /// True for states the server can never leave.
    #[inline]
    pub fn is_final(self) -> bool {
        matches!(self, Self::Stopped | Self::Terminal)
    }
}

/// Identity of one accepted connection, handed to the factory and to the
/// handler's `Connected` event.
#[derive(Clone, Debug)]
pub struct WsServerConnectionContext {
    /// Monotonic per-server connection counter, stable for the connection's life.
    pub connection_id: u64,
    pub peer_addr: SocketAddr,
    pub local_addr: SocketAddr,
}

/// Why a connection is being torn down. Delivered as the final handler event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsServerCloseCause {
    /// The peer sent a close frame or the stream ended.
    ClientClosed,
    /// The handler returned [`WsServerEndpointAction::Close`].
    HandlerRequested,
    /// The handler returned an error.
    HandlerError(String),
    /// The client fell behind the retained-command budget. It must reconnect to
    /// obtain an authoritative snapshot; stale frames are never queued.
    Lagged { missed: u64 },
    /// The server is shutting down.
    ServerShutdown,
    /// Transport read/write failure.
    TransportError(String),
}

/// An event delivered to a connection's endpoint handler.
///
/// Control frames (ping/pong/close) are owned by the transport and never
/// surface here; `Frame` carries application text/binary payloads only.
#[derive(Debug)]
pub enum WsServerEndpointEvent<'a, C> {
    /// Handshake completed. First event for every connection.
    Connected(&'a WsServerConnectionContext),
    /// An inbound application frame.
    Frame(&'a WsFrame),
    /// A managed command fanned out from the server handle.
    Command(&'a C),
    /// Periodic wakeup at `endpoint_tick_interval`.
    Tick,
    /// Final event for every connection. The returned action is ignored except
    /// for a close frame, which is best-effort.
    Closed(WsServerCloseCause),
}

/// Inline, allocation-free batch of frames.
///
/// Sized for the common "acknowledge plus a couple of snapshots" reply; larger
/// replies should be sent across successive events rather than heap-allocated
/// here.
pub const WS_SERVER_FRAME_BATCH_CAPACITY: usize = 4;

#[derive(Debug, Default)]
pub struct WsServerFrameBatch {
    frames: [Option<WsFrame>; WS_SERVER_FRAME_BATCH_CAPACITY],
    len: usize,
}

impl WsServerFrameBatch {
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a frame. Returns the frame back as `Err` when the batch is full,
    /// so a caller can never silently drop a reply.
    #[inline]
    pub fn push(&mut self, frame: WsFrame) -> Result<(), WsFrame> {
        if self.len >= WS_SERVER_FRAME_BATCH_CAPACITY {
            return Err(frame);
        }
        self.frames[self.len] = Some(frame);
        self.len += 1;
        Ok(())
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = WsFrame> + '_ {
        let len = self.len;
        self.len = 0;
        self.frames[..len].iter_mut().filter_map(Option::take)
    }
}

impl FromIterator<WsFrame> for WsServerFrameBatch {
    /// Frames beyond [`WS_SERVER_FRAME_BATCH_CAPACITY`] are not accepted; build
    /// batches from a bounded, known set.
    fn from_iter<I: IntoIterator<Item = WsFrame>>(iter: I) -> Self {
        let mut batch = Self::new();
        for frame in iter {
            if batch.push(frame).is_err() {
                break;
            }
        }
        batch
    }
}

/// What the transport should do after a handler event.
#[derive(Debug)]
pub enum WsServerEndpointAction {
    /// Do nothing.
    None,
    /// Send exactly one frame.
    Send(WsFrame),
    /// Send a small, inline batch of frames in order.
    SendBatch(WsServerFrameBatch),
    /// Close this connection, optionally with a close frame.
    Close(Option<WsCloseFrame>),
}

/// Per-connection application logic.
///
/// Implementations are concrete and statically dispatched: there is no `dyn`,
/// no `async_trait`, and no boxing on the event path.
pub trait WsServerEndpointHandler: Send + 'static {
    /// The managed command type fanned out by the server handle. Keep this
    /// cheap to clone — payloads should be reference-counted (`Bytes`) so that
    /// fanout to N connections clones N refcounts, not N payloads.
    type Command: Clone + Send + 'static;

    type Error: std::error::Error + Send + Sync + 'static;

    /// Handle one event. Returning `Err` closes **only this** connection; the
    /// accept loop and every sibling connection are unaffected.
    fn on_event(
        &mut self,
        event: WsServerEndpointEvent<'_, Self::Command>,
    ) -> impl Future<Output = Result<WsServerEndpointAction, Self::Error>> + Send;
}

/// Creates one concrete endpoint handler per accepted connection.
///
/// Only called after a successful WebSocket handshake, so a malformed upgrade
/// never constructs application state.
pub trait WsServerEndpointFactory: Send + Sync + 'static {
    type Handler: WsServerEndpointHandler;

    fn create_endpoint(&self, ctx: &WsServerConnectionContext) -> Self::Handler;
}

/// The command type fanned out by a server built from factory `F`.
pub type WsServerCommandOf<F> =
    <<F as WsServerEndpointFactory>::Handler as WsServerEndpointHandler>::Command;

/// Failure to start a managed server. Both variants are caller bugs, not
/// runtime conditions: neither can be repaired by retrying.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManagedWsServerStartError {
    #[error("invalid managed ws server configuration: {field}: {reason}")]
    InvalidConfig {
        field: &'static str,
        reason: &'static str,
    },
    #[error("managed ws server requires an active tokio runtime")]
    RuntimeUnavailable,
}

/// An unrecoverable server-loop failure. Retained for the life of the handle so
/// a supervisor observing late still learns the cause.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManagedWsServerTerminalError {
    #[error("bind to {addr} failed {attempts} consecutive times, retry budget exhausted: {error}")]
    BindRetryExhausted {
        addr: SocketAddr,
        attempts: u32,
        error: String,
    },
    #[error(
        "listener at {addr} was lost and rebinding failed {attempts} consecutive times, retry budget exhausted: {error}"
    )]
    AcceptRetryExhausted {
        addr: SocketAddr,
        attempts: u32,
        error: String,
    },
    #[error("managed ws server accept loop failed unrecoverably: {reason}")]
    InternalLoopFailure { reason: String },
}

/// Failure to fan out a managed command.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManagedWsServerDispatchError {
    #[error("managed ws server terminated: {0}")]
    Terminated(#[from] ManagedWsServerTerminalError),
    #[error("managed ws server is no longer running")]
    Stopped,
}

/// Failure to shut a managed server down within its budget.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManagedWsServerShutdownError {
    #[error("managed ws server did not shut down within {timeout:?}; connections were aborted")]
    Timeout { timeout: Duration },
}

/// Failure to observe an expected status within a deadline.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManagedWsServerStatusError {
    #[error(
        "managed ws server did not reach {expected:?} within {timeout:?}; last status {last:?}"
    )]
    Timeout {
        expected: ManagedWsServerStatus,
        last: ManagedWsServerStatus,
        timeout: Duration,
    },
    #[error("managed ws server settled in final status {last:?} and can never reach {expected:?}")]
    Settled {
        expected: ManagedWsServerStatus,
        last: ManagedWsServerStatus,
    },
}
