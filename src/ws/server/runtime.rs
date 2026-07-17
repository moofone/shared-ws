use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures_util::SinkExt;
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinSet;

use crate::client::{WsClient, accept_async_with_config};
use crate::core::frame::{WsCloseFrame, WsFrame};

use super::config::{WsServerConfig, WsServerRetryPolicy};
use super::handle::{ManagedWsServerHandle, ServerShared};
use super::types::{
    ManagedWsServerStartError, ManagedWsServerStatus, ManagedWsServerTerminalError,
    WsServerCloseCause, WsServerCommandOf, WsServerConnectionContext, WsServerEndpointAction,
    WsServerEndpointEvent, WsServerEndpointFactory, WsServerEndpointHandler,
};

/// WebSocket close codes used by the transport itself.
const CLOSE_GOING_AWAY: u16 = 1001;
const CLOSE_POLICY_VIOLATION: u16 = 1008;
const CLOSE_INTERNAL_ERROR: u16 = 1011;

/// Deterministic fault injection, settable only through the `test-support`
/// feature. Production callers get an all-`None` value, so no fault command
/// exists on the production API surface.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ServerFaults {
    /// Force the accept loop into a terminal failure once this many connections
    /// have been accepted (`Some(0)` fails as soon as the listener is bound).
    pub(crate) terminal_accept_failure_after: Option<u64>,
}

/// Deterministic accept-loop fault injection for tests.
///
/// This type only exists under the `test-support` feature and is the sole way
/// to reach [`ServerFaults`]; there is deliberately no production fault hook.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Default)]
pub struct WsServerTestFaults {
    /// Force the accept loop into a terminal failure once this many connections
    /// have been accepted (`Some(0)` fails as soon as the listener is bound).
    pub terminal_accept_failure_after: Option<u64>,
}

#[cfg(feature = "test-support")]
impl From<WsServerTestFaults> for ServerFaults {
    fn from(faults: WsServerTestFaults) -> Self {
        Self {
            terminal_accept_failure_after: faults.terminal_accept_failure_after,
        }
    }
}

/// Spawn a managed WebSocket server.
///
/// The returned handle owns the server task. The server binds
/// `config.bind_addr` with `SO_REUSEADDR`, retries under `retry` while the
/// address is unavailable (reporting [`ManagedWsServerStatus::Retrying`] rather
/// than failing), and creates one statically dispatched endpoint handler per
/// accepted connection via `factory`.
///
/// Errors here are configuration/environment faults only — a busy port is a
/// runtime condition the server self-heals from, not a start failure.
pub fn spawn_managed_ws_server_task<F>(
    config: WsServerConfig,
    factory: F,
    retry: WsServerRetryPolicy,
) -> Result<ManagedWsServerHandle<WsServerCommandOf<F>>, ManagedWsServerStartError>
where
    F: WsServerEndpointFactory,
{
    spawn_with_faults(config, factory, retry, ServerFaults::default())
}

/// [`spawn_managed_ws_server_task`] with deterministic fault injection.
#[cfg(feature = "test-support")]
pub fn spawn_managed_ws_server_task_with_faults<F>(
    config: WsServerConfig,
    factory: F,
    retry: WsServerRetryPolicy,
    faults: WsServerTestFaults,
) -> Result<ManagedWsServerHandle<WsServerCommandOf<F>>, ManagedWsServerStartError>
where
    F: WsServerEndpointFactory,
{
    spawn_with_faults(config, factory, retry, faults.into())
}

fn spawn_with_faults<F>(
    config: WsServerConfig,
    factory: F,
    retry: WsServerRetryPolicy,
    faults: ServerFaults,
) -> Result<ManagedWsServerHandle<WsServerCommandOf<F>>, ManagedWsServerStartError>
where
    F: WsServerEndpointFactory,
{
    config.validate()?;
    retry.validate()?;
    let runtime = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle,
        Err(_) => return Err(ManagedWsServerStartError::RuntimeUnavailable),
    };

    let (command_tx, _) = broadcast::channel(config.retained_commands_per_client);
    let (status_tx, _) = watch::channel(ManagedWsServerStatus::Starting);
    let (shutdown_tx, _) = watch::channel(false);
    let shared = Arc::new(ServerShared {
        command_tx,
        status_tx,
        shutdown_tx,
        terminal: OnceLock::new(),
        active: Arc::new(AtomicUsize::new(0)),
        next_connection_id: AtomicU64::new(0),
    });

    let task = runtime.spawn(run_server(
        config,
        Arc::new(factory),
        retry,
        faults,
        Arc::clone(&shared),
    ));
    Ok(ManagedWsServerHandle::new(shared, task))
}

/// Resolve once shutdown has been requested, including when it already was.
///
/// `watch::Receiver::wait_for` yields a read guard, which would be held across
/// the surrounding `select!` await and make the connection future non-`Send`;
/// this borrows only long enough to read the flag.
async fn wait_for_shutdown(shutdown_rx: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown_rx.borrow_and_update() {
            return;
        }
        if shutdown_rx.changed().await.is_err() {
            return;
        }
    }
}

/// Guard for one unit of the server-wide connection budget. Releasing it on
/// drop is what makes the cap self-correcting regardless of how a connection
/// ends (clean close, handler error, abort).
struct ConnectionSlot(Arc<AtomicUsize>);

impl ConnectionSlot {
    fn try_acquire(active: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        let claimed = active.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            if current >= max {
                None
            } else {
                Some(current + 1)
            }
        });
        match claimed {
            Ok(_) => Some(Self(Arc::clone(active))),
            Err(_) => None,
        }
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Why the accept loop returned.
enum AcceptOutcome {
    /// A shutdown was requested.
    Shutdown,
    /// Unrecoverable: the server must stop and report the cause.
    Terminal(ManagedWsServerTerminalError),
    /// The listener itself failed; the outer loop rebinds under the retry policy.
    ListenerLost(String),
}

/// Which retry budget a consecutive failure run belongs to. A failure before
/// the first successful bind is a bind problem; one after is a lost listener.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FailureOrigin {
    Bind,
    Accept,
}

/// CRITICAL PATH: server lifecycle. Owns bind/rebind with backoff, status
/// transitions, terminal classification, and shutdown draining. Every exit path
/// must land on exactly one of `Stopped` or `Terminal`.
async fn run_server<F>(
    config: WsServerConfig,
    factory: Arc<F>,
    retry: WsServerRetryPolicy,
    faults: ServerFaults,
    shared: Arc<ServerShared<WsServerCommandOf<F>>>,
) where
    F: WsServerEndpointFactory,
{
    let mut shutdown_rx = shared.shutdown_tx.subscribe();
    let mut clients: JoinSet<()> = JoinSet::new();
    let mut backoff = retry.initial_backoff;
    let mut consecutive_failures: u32 = 0;
    let mut origin = FailureOrigin::Bind;

    'lifecycle: loop {
        if *shutdown_rx.borrow_and_update() {
            break 'lifecycle;
        }

        let failure = match bind_listener(config.bind_addr, config.backlog) {
            Ok(listener) => {
                consecutive_failures = 0;
                backoff = retry.initial_backoff;
                origin = FailureOrigin::Accept;
                shared.set_status(ManagedWsServerStatus::Listening);
                match run_accept_loop(
                    &config,
                    &factory,
                    &shared,
                    &mut clients,
                    listener,
                    &mut shutdown_rx,
                    faults,
                )
                .await
                {
                    AcceptOutcome::Shutdown => break 'lifecycle,
                    AcceptOutcome::Terminal(error) => {
                        shared.set_terminal(error);
                        abort_clients(&mut clients).await;
                        return;
                    }
                    AcceptOutcome::ListenerLost(error) => error,
                }
            }
            Err(error) => error.to_string(),
        };

        consecutive_failures += 1;
        if retry.is_exhausted(consecutive_failures) {
            let error = match origin {
                FailureOrigin::Bind => ManagedWsServerTerminalError::BindRetryExhausted {
                    addr: config.bind_addr,
                    attempts: consecutive_failures,
                    error: failure,
                },
                FailureOrigin::Accept => ManagedWsServerTerminalError::AcceptRetryExhausted {
                    addr: config.bind_addr,
                    attempts: consecutive_failures,
                    error: failure,
                },
            };
            shared.set_terminal(error);
            abort_clients(&mut clients).await;
            return;
        }

        shared.set_status(ManagedWsServerStatus::Retrying);
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = wait_for_shutdown(&mut shutdown_rx) => break 'lifecycle,
        }
        backoff = retry.next_backoff(backoff);
    }

    shared.set_status(ManagedWsServerStatus::ShuttingDown);
    drain_clients(&mut clients, config.shutdown_timeout).await;
    shared.set_status(ManagedWsServerStatus::Stopped);
}

/// A single bind attempt. `SO_REUSEADDR` is set before `bind` so a restart can
/// reclaim an address left in `TIME_WAIT` by the previous incarnation.
fn bind_listener(addr: SocketAddr, backlog: u32) -> std::io::Result<TcpListener> {
    let socket = match addr {
        SocketAddr::V4(_) => TcpSocket::new_v4()?,
        SocketAddr::V6(_) => TcpSocket::new_v6()?,
    };
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(backlog)
}

/// Per-connection accept failures that say nothing about listener health.
fn is_transient_accept_error(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::Interrupted
            | ErrorKind::WouldBlock
            | ErrorKind::TimedOut
    ) {
        return true;
    }
    // EMFILE/ENFILE: the process is out of descriptors. The listener is fine;
    // the condition clears as connections close.
    matches!(error.raw_os_error(), Some(libc_emfile) if libc_emfile == 23 || libc_emfile == 24)
}

/// CRITICAL PATH: accept loop. Owns connection admission, the connection cap,
/// task reaping, and fault classification. A handler failure or a rejected
/// client must never break out of this loop.
async fn run_accept_loop<F>(
    config: &WsServerConfig,
    factory: &Arc<F>,
    shared: &Arc<ServerShared<WsServerCommandOf<F>>>,
    clients: &mut JoinSet<()>,
    listener: TcpListener,
    shutdown_rx: &mut watch::Receiver<bool>,
    faults: ServerFaults,
) -> AcceptOutcome
where
    F: WsServerEndpointFactory,
{
    let local_addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(error) => {
            return AcceptOutcome::ListenerLost(error.to_string());
        }
    };
    let mut accepted: u64 = 0;

    loop {
        if let Some(threshold) = faults.terminal_accept_failure_after
            && accepted >= threshold
        {
            return AcceptOutcome::Terminal(ManagedWsServerTerminalError::InternalLoopFailure {
                reason: format!("injected accept-loop fault after {accepted} accepted connections"),
            });
        }

        tokio::select! {
            biased;

            _ = wait_for_shutdown(shutdown_rx) => return AcceptOutcome::Shutdown,

            // Reap finished connection tasks so the JoinSet cannot grow
            // unbounded. Disabled while empty, which is why it precedes accept.
            Some(_joined) = clients.join_next() => {}

            incoming = listener.accept() => {
                let (stream, peer_addr) = match incoming {
                    Ok(pair) => pair,
                    Err(error) => {
                        if is_transient_accept_error(&error) {
                            continue;
                        }
                        return AcceptOutcome::ListenerLost(error.to_string());
                    }
                };
                accepted += 1;
                admit_connection(config, factory, shared, clients, stream, peer_addr, local_addr);
            }
        }
    }
}

/// Admit one accepted socket, or drop it when the server-wide cap is full.
///
/// A rejected client is closed immediately rather than queued: the cap is a
/// hard budget, and a client that waits behind it would only receive stale data.
fn admit_connection<F>(
    config: &WsServerConfig,
    factory: &Arc<F>,
    shared: &Arc<ServerShared<WsServerCommandOf<F>>>,
    clients: &mut JoinSet<()>,
    stream: TcpStream,
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
) where
    F: WsServerEndpointFactory,
{
    let Some(slot) = ConnectionSlot::try_acquire(&shared.active, config.max_connections) else {
        drop(stream);
        return;
    };
    let ctx = WsServerConnectionContext {
        connection_id: shared.next_connection_id(),
        peer_addr,
        local_addr,
    };
    // Subscribe before the handshake so no command dispatched during the
    // handshake can slip past this connection unobserved.
    let command_rx = shared.command_tx.subscribe();
    let shutdown_rx = shared.shutdown_tx.subscribe();
    clients.spawn(run_connection(
        stream,
        ctx,
        Arc::clone(factory),
        command_rx,
        *config,
        shutdown_rx,
        slot,
    ));
}

/// CRITICAL PATH: one connection's whole life — handshake, handler creation,
/// event loop, close. The `ConnectionSlot` is moved in so the budget is
/// released however this returns.
async fn run_connection<F>(
    stream: TcpStream,
    ctx: WsServerConnectionContext,
    factory: Arc<F>,
    mut command_rx: broadcast::Receiver<WsServerCommandOf<F>>,
    config: WsServerConfig,
    mut shutdown_rx: watch::Receiver<bool>,
    _slot: ConnectionSlot,
) where
    F: WsServerEndpointFactory,
{
    let handshake = accept_async_with_config(stream, config.websocket);
    let mut ws = match tokio::time::timeout(config.handshake_timeout, handshake).await {
        Ok(Ok(ws)) => ws,
        // A malformed or slow upgrade never constructs application state: the
        // factory is not called and no handler observes the socket.
        Ok(Err(_)) | Err(_) => return,
    };

    let mut handler = factory.create_endpoint(&ctx);
    let mut tick = tokio::time::interval(config.endpoint_tick_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` fires immediately on first tick; the connection has just been
    // established, so consume that one rather than waking the handler at once.
    tick.tick().await;

    let cause = run_connection_loop(
        &mut ws,
        &mut handler,
        &ctx,
        &mut command_rx,
        &mut tick,
        &mut shutdown_rx,
    )
    .await;
    finish_connection(&mut ws, &mut handler, cause).await;
}

/// CRITICAL PATH: the connection event loop. Ping/pong and close are answered
/// here and never reach the handler; a lagging client is closed rather than
/// served stale frames.
async fn run_connection_loop<H>(
    ws: &mut WsClient,
    handler: &mut H,
    ctx: &WsServerConnectionContext,
    command_rx: &mut broadcast::Receiver<H::Command>,
    tick: &mut tokio::time::Interval,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> WsServerCloseCause
where
    H: WsServerEndpointHandler,
{
    match handler
        .on_event(WsServerEndpointEvent::Connected(ctx))
        .await
    {
        Ok(action) => {
            if let Some(cause) = apply_action(ws, action).await {
                return cause;
            }
        }
        Err(error) => return WsServerCloseCause::HandlerError(error.to_string()),
    }

    loop {
        tokio::select! {
            biased;

            _ = wait_for_shutdown(shutdown_rx) => {
                return WsServerCloseCause::ServerShutdown;
            }

            incoming = ws.next() => {
                let frame = match incoming {
                    None => return WsServerCloseCause::ClientClosed,
                    Some(Err(error)) => {
                        return WsServerCloseCause::TransportError(error.to_string());
                    }
                    Some(Ok(frame)) => frame,
                };
                match frame {
                    WsFrame::Close(_) => return WsServerCloseCause::ClientClosed,
                    WsFrame::Ping(payload) => {
                        if let Err(error) = ws.send(WsFrame::Pong(payload)).await {
                            return WsServerCloseCause::TransportError(error.to_string());
                        }
                    }
                    // Unsolicited pong: the transport owns liveness, nothing to do.
                    WsFrame::Pong(_) => {}
                    application @ (WsFrame::Text(_) | WsFrame::Binary(_)) => {
                        match handler
                            .on_event(WsServerEndpointEvent::Frame(&application))
                            .await
                        {
                            Ok(action) => {
                                if let Some(cause) = apply_action(ws, action).await {
                                    return cause;
                                }
                            }
                            Err(error) => {
                                return WsServerCloseCause::HandlerError(error.to_string());
                            }
                        }
                    }
                }
            }

            command = command_rx.recv() => {
                match command {
                    Ok(command) => {
                        match handler.on_event(WsServerEndpointEvent::Command(&command)).await {
                            Ok(action) => {
                                if let Some(cause) = apply_action(ws, action).await {
                                    return cause;
                                }
                            }
                            Err(error) => {
                                return WsServerCloseCause::HandlerError(error.to_string());
                            }
                        }
                    }
                    // The client fell behind the retained budget. Closing is the
                    // contract: it must reconnect to get an authoritative
                    // snapshot instead of resuming from a gap.
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        return WsServerCloseCause::Lagged { missed };
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return WsServerCloseCause::ServerShutdown;
                    }
                }
            }

            _ = tick.tick() => {
                match handler.on_event(WsServerEndpointEvent::Tick).await {
                    Ok(action) => {
                        if let Some(cause) = apply_action(ws, action).await {
                            return cause;
                        }
                    }
                    Err(error) => return WsServerCloseCause::HandlerError(error.to_string()),
                }
            }
        }
    }
}

/// Apply a handler action. `Some(cause)` means the connection must close.
async fn apply_action(
    ws: &mut WsClient,
    action: WsServerEndpointAction,
) -> Option<WsServerCloseCause> {
    match action {
        WsServerEndpointAction::None => None,
        WsServerEndpointAction::Send(frame) => match ws.send(frame).await {
            Ok(()) => None,
            Err(error) => Some(WsServerCloseCause::TransportError(error.to_string())),
        },
        WsServerEndpointAction::SendBatch(mut batch) => {
            for frame in batch.drain() {
                if let Err(error) = ws.send(frame).await {
                    return Some(WsServerCloseCause::TransportError(error.to_string()));
                }
            }
            None
        }
        WsServerEndpointAction::Close(frame) => {
            let _ = ws.send(WsFrame::Close(frame)).await;
            Some(WsServerCloseCause::HandlerRequested)
        }
    }
}

/// Deliver the terminal `Closed` event and close the socket. Best-effort: the
/// peer may already be gone, which is not an error worth propagating.
async fn finish_connection<H>(ws: &mut WsClient, handler: &mut H, cause: WsServerCloseCause)
where
    H: WsServerEndpointHandler,
{
    let close_frame = match &cause {
        WsServerCloseCause::Lagged { .. } => Some(WsCloseFrame {
            code: CLOSE_POLICY_VIOLATION,
            reason: Bytes::from_static(b"client lagged; reconnect for an authoritative snapshot"),
        }),
        WsServerCloseCause::HandlerError(_) => Some(WsCloseFrame {
            code: CLOSE_INTERNAL_ERROR,
            reason: Bytes::from_static(b"endpoint handler error"),
        }),
        WsServerCloseCause::ServerShutdown => Some(WsCloseFrame {
            code: CLOSE_GOING_AWAY,
            reason: Bytes::from_static(b"server shutting down"),
        }),
        WsServerCloseCause::ClientClosed
        | WsServerCloseCause::HandlerRequested
        | WsServerCloseCause::TransportError(_) => None,
    };

    let _ = handler.on_event(WsServerEndpointEvent::Closed(cause)).await;
    if let Some(frame) = close_frame {
        let _ = ws.send(WsFrame::Close(Some(frame))).await;
    }
    let _ = SinkExt::close(ws).await;
}

/// Drain connection tasks within the budget, aborting whatever remains. Either
/// way no connection task outlives this call.
async fn drain_clients(clients: &mut JoinSet<()>, timeout: Duration) {
    let drained = tokio::time::timeout(timeout, async {
        while clients.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        abort_clients(clients).await;
    }
}

async fn abort_clients(clients: &mut JoinSet<()>) {
    clients.abort_all();
    while clients.join_next().await.is_some() {}
}
