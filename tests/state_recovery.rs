use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use bytes::Bytes;
use defi_ws::client::accept_async;
use defi_ws::transport::tungstenite::TungsteniteTransport;
use defi_ws::ws::{
    ProtocolPingPong, WebSocketActor, WebSocketActorArgs, WebSocketBufferConfig, WebSocketEvent,
    WsApplicationPingPong, WsDisconnectAction, WsDisconnectCause, WsErrorAction, WsEndpointHandler,
    WsMessage, WsMessageAction, WsParseOutcome, WsReconnectStrategy, WsSubscriptionAction,
    WsSubscriptionManager, WsSubscriptionStatus, WsTlsConfig,
};
use kameo::Actor;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[allow(dead_code)]
#[derive(Debug)]
enum ServerEvent {
    Connected { conn_id: usize },
    Disconnected { conn_id: usize },
    Data { conn_id: usize, bytes: Bytes },
    Ping { conn_id: usize },
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
enum ServerMode {
    RespondToPings,
    IgnorePings,
    CloseAfterDelay(Duration),
}

async fn spawn_ws_server(mode: ServerMode) -> (SocketAddr, mpsc::UnboundedReceiver<ServerEvent>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let mut conn_id = 0usize;
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            conn_id = conn_id.saturating_add(1);
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                let _ = tx.send(ServerEvent::Connected { conn_id });

                let close_after = match mode {
                    ServerMode::CloseAfterDelay(delay) => Some(delay),
                    _ => None,
                };
                if let Some(delay) = close_after {
                    let timer = tokio::time::sleep(delay);
                    tokio::pin!(timer);

                    loop {
                        tokio::select! {
                            _ = &mut timer => {
                                let _ = ws.send(WsMessage::Close(None)).await;
                                break;
                            }
                            message = ws.next() => {
                                let Some(message) = message else { break; };
                                match message {
                                    Ok(WsMessage::Ping(payload)) => {
                                        let _ = tx.send(ServerEvent::Ping { conn_id });
                                        if matches!(mode, ServerMode::RespondToPings) {
                                            let _ = ws.send(WsMessage::Pong(payload)).await;
                                        }
                                    }
                                    Ok(WsMessage::Binary(bytes)) => {
                                        let _ = tx.send(ServerEvent::Data { conn_id, bytes });
                                    }
                                    Ok(WsMessage::Text(text)) => {
                                        let _ = tx.send(ServerEvent::Data {
                                            conn_id,
                                            bytes: Bytes::copy_from_slice(text.as_ref()),
                                        });
                                    }
                                    Ok(WsMessage::Close(_)) => break,
                                    Err(_) => break,
                                    _ => {}
                                }
                            }
                        }
                    }
                } else {
                    while let Some(message) = ws.next().await {
                        match message {
                            Ok(WsMessage::Ping(payload)) => {
                                let _ = tx.send(ServerEvent::Ping { conn_id });
                                if matches!(mode, ServerMode::RespondToPings) {
                                    let _ = ws.send(WsMessage::Pong(payload)).await;
                                }
                            }
                            Ok(WsMessage::Binary(bytes)) => {
                                let _ = tx.send(ServerEvent::Data { conn_id, bytes });
                            }
                            Ok(WsMessage::Text(text)) => {
                                let _ = tx.send(ServerEvent::Data {
                                    conn_id,
                                    bytes: Bytes::copy_from_slice(text.as_ref()),
                                });
                            }
                            Ok(WsMessage::Close(_)) => break,
                            Err(_) => break,
                            _ => {}
                        }
                    }
                }
                let _ = tx.send(ServerEvent::Disconnected { conn_id });
            });
        }
    });

    (addr, rx)
}

#[derive(Debug, Default)]
struct Counters {
    opens: AtomicUsize,
    resets: AtomicUsize,
    desired_len: AtomicUsize,
}

#[derive(Clone, Debug)]
struct RecordingSubscriptions {
    desired: Vec<Vec<u8>>,
    counters: Arc<Counters>,
}

impl RecordingSubscriptions {
    fn new(initial: Vec<Vec<u8>>, counters: Arc<Counters>) -> Self {
        counters.desired_len.store(initial.len(), Ordering::Release);
        Self {
            desired: initial,
            counters,
        }
    }
}

impl WsSubscriptionManager for RecordingSubscriptions {
    type SubscriptionMessage = Vec<u8>;

    fn initial_subscriptions(&mut self) -> Vec<Self::SubscriptionMessage> {
        // NOTE: The current trait requires owned payloads. This forces cloning here.
        self.desired.clone()
    }

    fn update_subscriptions(
        &mut self,
        action: WsSubscriptionAction<Self::SubscriptionMessage>,
    ) -> Result<Vec<Self::SubscriptionMessage>, String> {
        match action {
            WsSubscriptionAction::Add(mut msgs) => {
                self.desired.append(&mut msgs);
            }
            WsSubscriptionAction::Remove(_) => {
                // Not needed for this test.
            }
            WsSubscriptionAction::Replace(msgs) => {
                self.desired = msgs;
            }
            WsSubscriptionAction::Clear => {
                self.desired.clear();
            }
        }
        self.counters
            .desired_len
            .store(self.desired.len(), Ordering::Release);
        // For this test we only care that desired state survives reconnect; we don't need to
        // send updates immediately.
        Ok(Vec::new())
    }

    fn serialize_subscription(&mut self, msg: &Self::SubscriptionMessage) -> Vec<u8> {
        msg.clone()
    }

    fn handle_subscription_response(&mut self, _data: &[u8]) -> WsSubscriptionStatus {
        WsSubscriptionStatus::NotSubscriptionResponse
    }
}

#[derive(Clone, Debug)]
struct RecordingHandler {
    counters: Arc<Counters>,
    subs: RecordingSubscriptions,
}

impl RecordingHandler {
    fn new(counters: Arc<Counters>, initial: Vec<Vec<u8>>) -> Self {
        Self {
            counters: counters.clone(),
            subs: RecordingSubscriptions::new(initial, counters),
        }
    }
}

#[derive(Clone)]
struct FastReconnect {
    delay: Duration,
}

impl WsReconnectStrategy for FastReconnect {
    fn next_delay(&mut self) -> Duration {
        self.delay
    }
    fn reset(&mut self) {}
    fn should_retry(&self) -> bool {
        true
    }
}

impl WsEndpointHandler for RecordingHandler {
    type Message = ();
    type Error = std::io::Error;
    type Subscription = RecordingSubscriptions;

    fn subscription_manager(&mut self) -> &mut Self::Subscription {
        &mut self.subs
    }

    fn generate_auth(&self) -> Option<Vec<u8>> {
        None
    }

    fn parse(&mut self, _data: &[u8]) -> Result<WsParseOutcome<Self::Message>, Self::Error> {
        Ok(WsParseOutcome::Message(WsMessageAction::Continue))
    }

    fn handle_message(&mut self, _msg: Self::Message) -> Result<(), Self::Error> {
        Ok(())
    }

    fn handle_server_error(
        &mut self,
        _code: Option<i32>,
        _message: &str,
        _data: Option<sonic_rs::Value>,
    ) -> WsErrorAction {
        WsErrorAction::Continue
    }

    fn reset_state(&mut self) {
        self.counters.resets.fetch_add(1, Ordering::Relaxed);
    }

    fn classify_disconnect(&self, _cause: &WsDisconnectCause) -> WsDisconnectAction {
        WsDisconnectAction::ImmediateReconnect
    }

    fn on_open(&mut self) {
        self.counters.opens.fetch_add(1, Ordering::Relaxed);
    }
}

async fn next_connection(
    rx: &mut mpsc::UnboundedReceiver<ServerEvent>,
    timeout: Duration,
) -> usize {
    tokio::time::timeout(timeout, async {
        loop {
            match rx.recv().await {
                Some(ServerEvent::Connected { conn_id }) => return conn_id,
                Some(_) => {}
                None => panic!("server event stream ended"),
            }
        }
    })
    .await
    .expect("timed out waiting for server connection")
}

async fn collect_data_for_conn(
    rx: &mut mpsc::UnboundedReceiver<ServerEvent>,
    conn_id: usize,
    want: usize,
    timeout: Duration,
) -> Vec<Bytes> {
    let mut out = Vec::new();
    tokio::time::timeout(timeout, async {
        while out.len() < want {
            match rx.recv().await {
                Some(ServerEvent::Data {
                    conn_id: got,
                    bytes,
                }) if got == conn_id => out.push(bytes),
                Some(_) => {}
                None => break,
            }
        }
    })
    .await
    .expect("timed out waiting for server data");
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actor_state_survives_remote_close_and_resubscribes() {
    let (addr, mut rx) = spawn_ws_server(ServerMode::CloseAfterDelay(Duration::from_millis(75)))
        .await;

    let counters = Arc::new(Counters::default());
    let handler = RecordingHandler::new(counters.clone(), vec![b"sub0".to_vec()]);

    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: format!("ws://{}", addr),
        tls: WsTlsConfig::default(),
        transport: TungsteniteTransport::default(),
        reconnect_strategy: FastReconnect {
            delay: Duration::from_millis(10),
        },
        handler,
        ingress: defi_ws::core::ForwardAllIngress::default(),
        ping_strategy: ProtocolPingPong::new(Duration::from_secs(60), Duration::from_secs(60)),
        enable_ping: false,
        stale_threshold: Duration::from_secs(30),
        ws_buffers: WebSocketBufferConfig::default(),
        outbound_capacity: 32,
        rate_limiter: None,
        circuit_breaker: None,
        latency_policy: None,
        registration: None,
        metrics: None,
    });

    actor.tell(WebSocketEvent::Connect).send().await.unwrap();

    let first = next_connection(&mut rx, Duration::from_secs(2)).await;

    // Apply an update while connected; it should persist across reconnect even if not sent yet.
    actor
        .tell(defi_ws::ws::WsSubscriptionUpdate {
            action: WsSubscriptionAction::Add(vec![b"sub1".to_vec()]),
        })
        .send()
        .await
        .unwrap();

    // Initial connection should see the baseline subscription.
    let first_msgs = collect_data_for_conn(&mut rx, first, 1, Duration::from_secs(2)).await;
    assert_eq!(first_msgs[0].as_ref(), b"sub0");

    // After remote close, actor should reconnect and re-send desired subscriptions.
    let second = next_connection(&mut rx, Duration::from_secs(2)).await;
    let second_msgs = collect_data_for_conn(&mut rx, second, 2, Duration::from_secs(2)).await;
    let mut payloads: Vec<Vec<u8>> = second_msgs
        .into_iter()
        .map(|b| b.as_ref().to_vec())
        .collect();
    payloads.sort();
    assert_eq!(payloads, vec![b"sub0".to_vec(), b"sub1".to_vec()]);

    assert_eq!(counters.opens.load(Ordering::Acquire), 2);
    assert_eq!(counters.resets.load(Ordering::Acquire), 0);
    assert_eq!(counters.desired_len.load(Ordering::Acquire), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actor_self_heals_after_stale_ping_and_resubscribes() {
    let (addr, mut rx) = spawn_ws_server(ServerMode::IgnorePings).await;

    let counters = Arc::new(Counters::default());
    let handler = RecordingHandler::new(counters.clone(), vec![b"sub0".to_vec()]);

    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: format!("ws://{}", addr),
        tls: WsTlsConfig::default(),
        transport: TungsteniteTransport::default(),
        reconnect_strategy: FastReconnect {
            delay: Duration::from_millis(10),
        },
        handler,
        // Use application-level heartbeat to avoid tungstenite's protocol-level auto-pong behavior.
        // interval > timeout ensures we trip stale after the first ping goes unanswered.
        ingress: defi_ws::core::ForwardAllIngress::default(),
        ping_strategy: WsApplicationPingPong::new(
            Duration::from_millis(60),
            Duration::from_millis(20),
            || (Bytes::from_static(b"ping"), "k".to_string()),
            |_bytes: &Bytes| None,
        ),
        enable_ping: true,
        stale_threshold: Duration::from_secs(30),
        ws_buffers: WebSocketBufferConfig::default(),
        outbound_capacity: 32,
        rate_limiter: None,
        circuit_breaker: None,
        latency_policy: None,
        registration: None,
        metrics: None,
    });

    actor.tell(WebSocketEvent::Connect).send().await.unwrap();
    let first = next_connection(&mut rx, Duration::from_secs(2)).await;
    let first_msgs = collect_data_for_conn(&mut rx, first, 1, Duration::from_secs(2)).await;
    assert_eq!(first_msgs[0].as_ref(), b"sub0");

    // Update desired state before the stale disconnect fires.
    actor
        .tell(defi_ws::ws::WsSubscriptionUpdate {
            action: WsSubscriptionAction::Add(vec![b"sub1".to_vec()]),
        })
        .send()
        .await
        .unwrap();

    // The server will see pings but won't respond; the actor should disconnect itself and reconnect.
    let second = next_connection(&mut rx, Duration::from_secs(2)).await;
    let second_msgs = collect_data_for_conn(&mut rx, second, 2, Duration::from_secs(2)).await;
    let mut payloads: Vec<Vec<u8>> = second_msgs
        .into_iter()
        .map(|b| b.as_ref().to_vec())
        .collect();
    payloads.sort();
    assert_eq!(payloads, vec![b"sub0".to_vec(), b"sub1".to_vec()]);

    assert_eq!(counters.opens.load(Ordering::Acquire), 2);
    assert_eq!(counters.resets.load(Ordering::Acquire), 0);
    assert_eq!(counters.desired_len.load(Ordering::Acquire), 2);
}
