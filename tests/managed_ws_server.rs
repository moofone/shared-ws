//! Real-local-socket integration tests for the generic managed WebSocket server.
//!
//! These tests exercise the complete boundary: `shared_ws::ws::server` owns the
//! listener, accept/rebind loop, handshake, per-connection tasks, ping/pong,
//! close, and command fanout; `shared_ws::client` drives the other side.

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use shared_ws::ws::server::{
    ManagedWsServerStatus, WsServerConfig, WsServerConnectionContext, WsServerEndpointAction,
    WsServerEndpointEvent, WsServerEndpointFactory, WsServerEndpointHandler, WsServerRetryPolicy,
    spawn_managed_ws_server_task,
};
use shared_ws::ws::{WsFrame, frame_bytes};

// ---------------------------------------------------------------------------
// Deterministic test endpoint handler
// ---------------------------------------------------------------------------

/// One managed command. `payload` is shared `Bytes`, so fanout to N clients
/// clones only a refcount — never the payload bytes.
#[derive(Clone, Debug)]
struct TestCommand {
    target: Option<&'static str>,
    payload: Bytes,
}

#[derive(Debug, thiserror::Error)]
enum TestHandlerError {
    #[error("test handler rejected frame: {0}")]
    Rejected(String),
}

#[derive(Default)]
struct TestObservations {
    connected: AtomicUsize,
    closed: AtomicUsize,
    ticks: AtomicUsize,
    /// Incremented once a handler is parked on the gate, so a test can dispatch
    /// into a connection that is provably stuck.
    blocked: AtomicUsize,
    /// Address of the payload each client observed, to prove single allocation.
    payload_ptrs: std::sync::Mutex<Vec<usize>>,
}

struct TestHandler {
    ctx: WsServerConnectionContext,
    obs: Arc<TestObservations>,
    /// When set, the handler errors on the first application frame it receives.
    fail_on_frame: bool,
    /// Gate that lets a test hold a handler inside `on_event` to force broadcast lag.
    gate: Option<Arc<tokio::sync::Notify>>,
    subscribed_target: Option<String>,
}

impl WsServerEndpointHandler for TestHandler {
    type Command = TestCommand;
    type Error = TestHandlerError;

    async fn on_event(
        &mut self,
        event: WsServerEndpointEvent<'_, Self::Command>,
    ) -> Result<WsServerEndpointAction, Self::Error> {
        match event {
            WsServerEndpointEvent::Connected(ctx) => {
                self.obs.connected.fetch_add(1, Ordering::SeqCst);
                let hello = format!(r#"{{"type":"hello","id":{}}}"#, ctx.connection_id);
                Ok(WsServerEndpointAction::Send(shared_ws::ws::into_ws_frame(
                    hello,
                )))
            }
            WsServerEndpointEvent::Frame(frame) => {
                if self.fail_on_frame {
                    return Err(TestHandlerError::Rejected("configured to fail".to_string()));
                }
                let Some(bytes) = frame_bytes(frame) else {
                    return Ok(WsServerEndpointAction::None);
                };
                // A frame is a subscription: remember the target we filter on.
                let text = String::from_utf8_lossy(bytes).to_string();
                if text.is_empty() {
                    return Err(TestHandlerError::Rejected("empty subscribe".to_string()));
                }
                self.subscribed_target = Some(text.clone());
                Ok(WsServerEndpointAction::Send(shared_ws::ws::into_ws_frame(
                    format!(r#"{{"type":"ack","target":"{text}"}}"#),
                )))
            }
            WsServerEndpointEvent::Command(command) => {
                if let Some(gate) = self.gate.clone() {
                    // Hold this connection inside the handler so the broadcast
                    // channel overflows deterministically behind it. `enable()`
                    // registers interest *before* the test is told we are parked,
                    // so `notify_waiters()` can never fire into a gap.
                    let parked = gate.notified();
                    tokio::pin!(parked);
                    parked.as_mut().enable();
                    self.obs.blocked.fetch_add(1, Ordering::SeqCst);
                    parked.await;
                }
                self.obs
                    .payload_ptrs
                    .lock()
                    .expect("payload ptr lock")
                    .push(command.payload.as_ptr() as usize);
                if let (Some(target), Some(subscribed)) =
                    (command.target, self.subscribed_target.as_deref())
                    && target != subscribed
                {
                    return Ok(WsServerEndpointAction::None);
                }
                Ok(WsServerEndpointAction::Send(WsFrame::Binary(
                    command.payload.clone(),
                )))
            }
            WsServerEndpointEvent::Tick => {
                self.obs.ticks.fetch_add(1, Ordering::SeqCst);
                Ok(WsServerEndpointAction::None)
            }
            WsServerEndpointEvent::Closed(_) => {
                self.obs.closed.fetch_add(1, Ordering::SeqCst);
                let _ = &self.ctx;
                Ok(WsServerEndpointAction::None)
            }
        }
    }
}

struct TestFactory {
    obs: Arc<TestObservations>,
    fail_on_frame: bool,
    gate: Option<Arc<tokio::sync::Notify>>,
}

impl TestFactory {
    fn new(obs: Arc<TestObservations>) -> Self {
        Self {
            obs,
            fail_on_frame: false,
            gate: None,
        }
    }
}

impl WsServerEndpointFactory for TestFactory {
    type Handler = TestHandler;

    fn create_endpoint(&self, ctx: &WsServerConnectionContext) -> Self::Handler {
        TestHandler {
            ctx: ctx.clone(),
            obs: Arc::clone(&self.obs),
            fail_on_frame: self.fail_on_frame,
            gate: self.gate.clone(),
            subscribed_target: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Reserve a currently-free loopback port.
fn free_addr() -> SocketAddr {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    addr
}

fn test_config(addr: SocketAddr) -> WsServerConfig {
    let mut config = WsServerConfig::new(addr);
    config.endpoint_tick_interval = Duration::from_millis(50);
    config.handshake_timeout = Duration::from_secs(5);
    config
}

fn fast_retry() -> WsServerRetryPolicy {
    WsServerRetryPolicy {
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(50),
        ..WsServerRetryPolicy::default()
    }
}

async fn connect(addr: SocketAddr) -> shared_ws::client::WsClient {
    shared_ws::client::connect_async(format!("ws://{addr}"))
        .await
        .expect("client connect")
}

/// Read the next application (non-control) frame.
async fn next_payload(client: &mut shared_ws::client::WsClient) -> Vec<u8> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .expect("frame timeout")
            .expect("stream open")
            .expect("frame ok");
        if let Some(bytes) = frame_bytes(&frame)
            && matches!(frame, WsFrame::Text(_) | WsFrame::Binary(_))
        {
            return bytes.to_vec();
        }
    }
}

// ---------------------------------------------------------------------------
// Scenario 1 — occupied address transitions to Retrying, then Listening
// ---------------------------------------------------------------------------

#[tokio::test]
async fn occupied_address_retries_then_listens_after_release() {
    let squatter = StdTcpListener::bind("127.0.0.1:0").expect("squat bind");
    let addr = squatter.local_addr().expect("addr");

    let obs = Arc::new(TestObservations::default());
    let handle = spawn_managed_ws_server_task(
        test_config(addr),
        TestFactory::new(Arc::clone(&obs)),
        fast_retry(),
    )
    .expect("spawn managed server");

    handle
        .wait_for_status(ManagedWsServerStatus::Retrying, Duration::from_secs(5))
        .await
        .expect("server must report Retrying while the address is occupied");

    // Releasing the address must let the server self-heal into Listening.
    drop(squatter);

    handle
        .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(10))
        .await
        .expect("server must rebind and reach Listening after the address is released");

    let mut client = connect(addr).await;
    let hello = next_payload(&mut client).await;
    assert!(
        String::from_utf8_lossy(&hello).contains("hello"),
        "recovered server must serve clients"
    );
    assert!(handle.terminal_error().is_none(), "retry is not terminal");
}

// ---------------------------------------------------------------------------
// Scenario 2 — multiple simultaneous clients, statically dispatched handlers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multiple_simultaneous_clients_use_static_endpoint_handlers() {
    let addr = free_addr();
    let obs = Arc::new(TestObservations::default());
    let handle = spawn_managed_ws_server_task(
        test_config(addr),
        TestFactory::new(Arc::clone(&obs)),
        fast_retry(),
    )
    .expect("spawn");
    handle
        .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(5))
        .await
        .expect("listening");

    let mut clients = Vec::new();
    for _ in 0..8 {
        let mut client = connect(addr).await;
        let hello = next_payload(&mut client).await;
        assert!(String::from_utf8_lossy(&hello).contains("hello"));
        clients.push(client);
    }

    assert_eq!(obs.connected.load(Ordering::SeqCst), 8);
    assert_eq!(handle.active_connection_count(), 8);
}

// ---------------------------------------------------------------------------
// Scenario 3 — handler error closes only that connection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn handler_error_closes_only_that_connection() {
    let addr = free_addr();
    let obs = Arc::new(TestObservations::default());
    let factory = TestFactory {
        obs: Arc::clone(&obs),
        fail_on_frame: true,
        gate: None,
    };
    let handle =
        spawn_managed_ws_server_task(test_config(addr), factory, fast_retry()).expect("spawn");
    handle
        .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(5))
        .await
        .expect("listening");

    let mut doomed = connect(addr).await;
    let _ = next_payload(&mut doomed).await;
    let mut survivor = connect(addr).await;
    let _ = next_payload(&mut survivor).await;

    // Trigger the handler error on `doomed` only.
    doomed
        .send(shared_ws::ws::into_ws_frame("boom".to_string()))
        .await
        .expect("send");

    // The failing connection is closed...
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match doomed.next().await {
                None | Some(Ok(WsFrame::Close(_))) => return true,
                Some(Err(_)) => return true,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .expect("doomed client must be closed by the server");
    assert!(closed);

    // ...and the accept loop plus the sibling connection are unaffected.
    handle
        .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(2))
        .await
        .expect("accept loop must survive a handler error");
    let dispatched = handle
        .dispatch(TestCommand {
            target: None,
            payload: Bytes::from_static(b"still-alive"),
        })
        .expect("dispatch after handler error");
    assert_eq!(dispatched, 1, "only the surviving client remains");
    let payload = next_payload(&mut survivor).await;
    assert_eq!(payload, b"still-alive");

    let mut late = connect(addr).await;
    let hello = next_payload(&mut late).await;
    assert!(
        String::from_utf8_lossy(&hello).contains("hello"),
        "server must still accept new clients"
    );
}

// ---------------------------------------------------------------------------
// Scenario 4 — connection cap is total per server instance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn connection_cap_rejects_excess_clients() {
    let addr = free_addr();
    let obs = Arc::new(TestObservations::default());
    let mut config = test_config(addr);
    config.max_connections = 2;
    let handle =
        spawn_managed_ws_server_task(config, TestFactory::new(Arc::clone(&obs)), fast_retry())
            .expect("spawn");
    handle
        .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(5))
        .await
        .expect("listening");

    let mut first = connect(addr).await;
    let _ = next_payload(&mut first).await;
    let mut second = connect(addr).await;
    let _ = next_payload(&mut second).await;
    assert_eq!(handle.active_connection_count(), 2);

    // The third connection is rejected by the cap, not queued.
    let rejected = tokio::time::timeout(Duration::from_secs(5), async {
        match shared_ws::client::connect_async(format!("ws://{addr}")).await {
            Err(_) => true,
            Ok(mut client) => matches!(client.next().await, None | Some(Ok(WsFrame::Close(_)))),
        }
    })
    .await
    .expect("third connection must be resolved by the cap, not hang");
    assert!(rejected, "connection beyond the cap must be rejected");

    assert_eq!(
        handle.active_connection_count(),
        2,
        "cap is a total per-server limit"
    );
    // Freeing a slot lets a new client in.
    drop(first);
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.active_connection_count() != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("closed client must free its slot");
    let mut third = connect(addr).await;
    let hello = next_payload(&mut third).await;
    assert!(String::from_utf8_lossy(&hello).contains("hello"));
}

// ---------------------------------------------------------------------------
// Scenario 5 — ping/pong is transport-owned
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ping_pong_is_transport_owned_and_never_reaches_the_handler() {
    let addr = free_addr();
    let obs = Arc::new(TestObservations::default());
    let handle = spawn_managed_ws_server_task(
        test_config(addr),
        TestFactory::new(Arc::clone(&obs)),
        fast_retry(),
    )
    .expect("spawn");
    handle
        .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(5))
        .await
        .expect("listening");

    let mut client = connect(addr).await;
    let _ = next_payload(&mut client).await;

    client
        .send(WsFrame::Ping(Bytes::from_static(b"pingpayload")))
        .await
        .expect("send ping");

    let pong = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(WsFrame::Pong(bytes))) => return bytes,
                Some(Ok(_)) => continue,
                other => panic!("expected pong, got {other:?}"),
            }
        }
    })
    .await
    .expect("server must answer ping with pong");
    assert_eq!(pong.as_ref(), b"pingpayload", "pong must echo the payload");

    // The handler never saw the control frame as an application frame.
    handle
        .dispatch(TestCommand {
            target: None,
            payload: Bytes::from_static(b"after-ping"),
        })
        .expect("dispatch");
    assert_eq!(next_payload(&mut client).await, b"after-ping");
}

// ---------------------------------------------------------------------------
// Scenario 6 — client close is transport-owned
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_close_is_transport_owned_and_reaps_the_connection() {
    let addr = free_addr();
    let obs = Arc::new(TestObservations::default());
    let handle = spawn_managed_ws_server_task(
        test_config(addr),
        TestFactory::new(Arc::clone(&obs)),
        fast_retry(),
    )
    .expect("spawn");
    handle
        .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(5))
        .await
        .expect("listening");

    let mut client = connect(addr).await;
    let _ = next_payload(&mut client).await;
    assert_eq!(handle.active_connection_count(), 1);

    client.send(WsFrame::Close(None)).await.expect("send close");

    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.active_connection_count() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("server must reap the connection after a client close");
    assert_eq!(obs.closed.load(Ordering::SeqCst), 1, "handler saw Closed");
}

// ---------------------------------------------------------------------------
// Scenario 7 — malformed handshake is rejected, accept loop survives
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_handshake_is_rejected_without_terminating_accept_loop() {
    use tokio::io::AsyncWriteExt;

    let addr = free_addr();
    let obs = Arc::new(TestObservations::default());
    let handle = spawn_managed_ws_server_task(
        test_config(addr),
        TestFactory::new(Arc::clone(&obs)),
        fast_retry(),
    )
    .expect("spawn");
    handle
        .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(5))
        .await
        .expect("listening");

    // Raw garbage: not a WebSocket upgrade.
    let mut junk = tokio::net::TcpStream::connect(addr).await.expect("tcp");
    junk.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .expect("write junk");
    junk.shutdown().await.ok();
    drop(junk);

    // The accept loop is untouched and the handler was never constructed.
    handle
        .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(2))
        .await
        .expect("accept loop must survive a malformed handshake");
    assert!(handle.terminal_error().is_none());

    let mut client = connect(addr).await;
    let hello = next_payload(&mut client).await;
    assert!(String::from_utf8_lossy(&hello).contains("hello"));
    assert_eq!(
        obs.connected.load(Ordering::SeqCst),
        1,
        "malformed handshake must not create an endpoint handler"
    );
}

// ---------------------------------------------------------------------------
// Scenario 8 — lagging client is closed, stale frames are never queued
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lagging_client_is_closed_rather_than_queued() {
    let addr = free_addr();
    let obs = Arc::new(TestObservations::default());
    let gate = Arc::new(tokio::sync::Notify::new());
    let factory = TestFactory {
        obs: Arc::clone(&obs),
        fail_on_frame: false,
        gate: Some(Arc::clone(&gate)),
    };
    let handle =
        spawn_managed_ws_server_task(test_config(addr), factory, fast_retry()).expect("spawn");
    handle
        .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(5))
        .await
        .expect("listening");

    let mut client = connect(addr).await;
    let _ = next_payload(&mut client).await;

    // Park the handler inside `on_event` on the first command...
    handle
        .dispatch(TestCommand {
            target: None,
            payload: Bytes::from_static(b"first"),
        })
        .expect("dispatch");
    tokio::time::timeout(Duration::from_secs(5), async {
        while obs.blocked.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("handler must park on the gate");

    // ...then overflow the single retained slot behind it.
    for i in 0..4u8 {
        handle
            .dispatch(TestCommand {
                target: None,
                payload: Bytes::from(vec![i]),
            })
            .expect("dispatch");
    }
    // Release the stuck handler; it must observe the overflow and be closed.
    gate.notify_waiters();

    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                None | Some(Ok(WsFrame::Close(_))) | Some(Err(_)) => return true,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .expect("a lagging client must be closed, never served stale queued frames");
    assert!(closed);

    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.active_connection_count() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("lagging client must be reaped");

    // The server itself is healthy and a fresh client gets an authoritative start.
    handle
        .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(2))
        .await
        .expect("server survives a lagging client");
}

// ---------------------------------------------------------------------------
// Scenario 9 — fanout semantics: zero clients, targeting, single allocation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dispatch_with_zero_clients_is_ok_zero() {
    let addr = free_addr();
    let obs = Arc::new(TestObservations::default());
    let handle = spawn_managed_ws_server_task(
        test_config(addr),
        TestFactory::new(Arc::clone(&obs)),
        fast_retry(),
    )
    .expect("spawn");
    handle
        .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(5))
        .await
        .expect("listening");

    let delivered = handle
        .dispatch(TestCommand {
            target: None,
            payload: Bytes::from_static(b"nobody-listening"),
        })
        .expect("dispatch with no clients must not be an error");
    assert_eq!(delivered, 0);
}

#[tokio::test]
async fn fanout_targets_matching_clients_and_clones_only_refcounts() {
    let addr = free_addr();
    let obs = Arc::new(TestObservations::default());
    let handle = spawn_managed_ws_server_task(
        test_config(addr),
        TestFactory::new(Arc::clone(&obs)),
        fast_retry(),
    )
    .expect("spawn");
    handle
        .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(5))
        .await
        .expect("listening");

    let mut alpha = connect(addr).await;
    let _ = next_payload(&mut alpha).await;
    alpha
        .send(shared_ws::ws::into_ws_frame("alpha".to_string()))
        .await
        .expect("subscribe alpha");
    assert!(String::from_utf8_lossy(&next_payload(&mut alpha).await).contains("ack"));

    let mut beta = connect(addr).await;
    let _ = next_payload(&mut beta).await;
    beta.send(shared_ws::ws::into_ws_frame("beta".to_string()))
        .await
        .expect("subscribe beta");
    assert!(String::from_utf8_lossy(&next_payload(&mut beta).await).contains("ack"));

    // A payload serialized exactly once, fanned out to both clients.
    let payload = Bytes::from_static(b"universal-event");
    let delivered = handle
        .dispatch(TestCommand {
            target: None,
            payload: payload.clone(),
        })
        .expect("dispatch");
    assert_eq!(delivered, 2);
    assert_eq!(next_payload(&mut alpha).await, b"universal-event");
    assert_eq!(next_payload(&mut beta).await, b"universal-event");

    // A targeted payload reaches only the matching client.
    handle
        .dispatch(TestCommand {
            target: Some("alpha"),
            payload: Bytes::from_static(b"alpha-only"),
        })
        .expect("dispatch");
    assert_eq!(next_payload(&mut alpha).await, b"alpha-only");

    handle
        .dispatch(TestCommand {
            target: Some("beta"),
            payload: Bytes::from_static(b"beta-only"),
        })
        .expect("dispatch");
    assert_eq!(next_payload(&mut beta).await, b"beta-only");

    // Both connections observed the *same* allocation for the universal event:
    // fanout clones a refcount, it does not re-serialize per connection.
    let ptrs = obs.payload_ptrs.lock().expect("ptrs");
    let universal_ptr = payload.as_ptr() as usize;
    let observed = ptrs.iter().filter(|p| **p == universal_ptr).count();
    assert_eq!(
        observed, 2,
        "each client must see the identical shared payload allocation"
    );
}
