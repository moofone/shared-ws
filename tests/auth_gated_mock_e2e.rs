use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Sink;
use futures_util::stream;
use shared_ws::transport::{WsTransport, WsTransportConnectFuture};
use shared_ws::ws::{
    ForwardAllIngress, GetConnectionStatus, ProtocolPingPong, WebSocketActor, WebSocketActorArgs,
    WebSocketBufferConfig, WebSocketError, WebSocketEvent, WsDisconnectAction, WsDisconnectCause,
    WsEndpointHandler, WsErrorAction, WsFrame, WsMessageAction, WsParseOutcome,
    WsReconnectStrategy, WsReplaySubscriptions, WsSessionMode, WsSetAuthenticated,
    WsSubscriptionAction, WsSubscriptionManager, WsSubscriptionStatus, message_bytes,
};
use tokio::sync::mpsc;

#[derive(Clone)]
struct NoReconnect;

impl WsReconnectStrategy for NoReconnect {
    fn next_delay(&mut self) -> Duration {
        Duration::from_secs(365 * 24 * 60 * 60)
    }
    fn reset(&mut self) {}
    fn should_retry(&self) -> bool {
        false
    }
}

#[derive(Clone)]
struct FastReconnect;

impl WsReconnectStrategy for FastReconnect {
    fn next_delay(&mut self) -> Duration {
        Duration::from_millis(5)
    }
    fn reset(&mut self) {}
    fn should_retry(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone)]
enum EndpointMsg {
    Opened { is_reconnect: bool },
}

#[derive(Debug, Clone)]
struct RecordingSubscriptions {
    desired: Vec<Vec<u8>>,
}

impl RecordingSubscriptions {
    fn new(initial: Vec<Vec<u8>>) -> Self {
        Self { desired: initial }
    }
}

impl WsSubscriptionManager for RecordingSubscriptions {
    type SubscriptionMessage = Vec<u8>;

    fn initial_subscriptions(&mut self) -> Vec<Self::SubscriptionMessage> {
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
            WsSubscriptionAction::Remove(_) => {}
            WsSubscriptionAction::Replace(msgs) => {
                self.desired = msgs;
            }
            WsSubscriptionAction::Clear => {
                self.desired.clear();
            }
        }
        Ok(Vec::new())
    }

    fn serialize_subscription(&mut self, msg: &Self::SubscriptionMessage) -> Vec<u8> {
        msg.clone()
    }

    fn handle_subscription_response(&mut self, _data: &[u8]) -> WsSubscriptionStatus {
        WsSubscriptionStatus::NotSubscriptionResponse
    }
}

#[derive(Clone)]
struct AuthGatedHandler {
    subs: RecordingSubscriptions,
    open_tx: mpsc::UnboundedSender<bool>,
}

impl AuthGatedHandler {
    fn new(initial_subs: Vec<Vec<u8>>, open_tx: mpsc::UnboundedSender<bool>) -> Self {
        Self {
            subs: RecordingSubscriptions::new(initial_subs),
            open_tx,
        }
    }
}

impl WsEndpointHandler for AuthGatedHandler {
    type Message = EndpointMsg;
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

    fn handle_message(&mut self, msg: Self::Message) -> Result<(), Self::Error> {
        match msg {
            EndpointMsg::Opened { is_reconnect } => {
                let _ = self.open_tx.send(is_reconnect);
            }
        }
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

    fn reset_state(&mut self) {}

    fn classify_disconnect(&self, _cause: &WsDisconnectCause) -> WsDisconnectAction {
        WsDisconnectAction::ImmediateReconnect
    }

    fn session_mode(&self) -> WsSessionMode {
        WsSessionMode::AuthGated
    }

    fn on_connection_opened(&mut self, is_reconnect: bool) -> Option<Self::Message> {
        Some(EndpointMsg::Opened { is_reconnect })
    }
}

#[derive(Clone)]
struct StubTransport {
    sent_tx: mpsc::UnboundedSender<WsFrame>,
}

impl WsTransport for StubTransport {
    type Reader = stream::Pending<Result<WsFrame, WebSocketError>>;
    type Writer = StubWriter;

    fn connect(
        &self,
        _url: String,
        _buffers: WebSocketBufferConfig,
    ) -> WsTransportConnectFuture<Self::Reader, Self::Writer> {
        let tx = self.sent_tx.clone();
        Box::pin(async move { Ok((stream::pending(), StubWriter { sent_tx: tx })) })
    }
}

struct StubWriter {
    sent_tx: mpsc::UnboundedSender<WsFrame>,
}

impl Sink<WsFrame> for StubWriter {
    type Error = WebSocketError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: WsFrame) -> Result<(), Self::Error> {
        let this = self.get_mut();
        let _ = this.sent_tx.send(item);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

async fn wait_connected<E, R, P, I, T>(
    actor: &kameo::prelude::ActorRef<WebSocketActor<E, R, P, I, T>>,
    timeout: Duration,
) where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: shared_ws::ws::WsPingPongStrategy,
    I: shared_ws::ws::WsIngress<Message = E::Message>,
    T: shared_ws::transport::WsTransport,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let status = actor.ask(GetConnectionStatus).await.unwrap();
        if matches!(status, shared_ws::ws::WsConnectionStatus::Connected) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for Connected status (last={status:?})");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn frame_payload(frame: &WsFrame) -> Vec<u8> {
    message_bytes(frame)
        .expect("frame should have payload")
        .to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_gated_does_not_replay_before_auth_and_replays_after_auth() {
    let (sent_tx, mut sent_rx) = mpsc::unbounded_channel::<WsFrame>();
    let (open_tx, mut open_rx) = mpsc::unbounded_channel::<bool>();

    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: "ws://stub".to_string(),
        transport: StubTransport { sent_tx },
        reconnect_strategy: NoReconnect,
        handler: AuthGatedHandler::new(vec![b"sub.private.a".to_vec()], open_tx),
        ingress: ForwardAllIngress::default(),
        ping_strategy: ProtocolPingPong::new(Duration::from_secs(60), Duration::from_secs(60)),
        enable_ping: false,
        stale_threshold: Duration::from_secs(60),
        ws_buffers: WebSocketBufferConfig::default(),
        outbound_capacity: 32,
        circuit_breaker: None,
        latency_policy: None,
        payload_latency_sampling: None,
        registration: None,
        metrics: None,
    });

    actor.tell(WebSocketEvent::Connect).send().await.unwrap();
    wait_connected(&actor, Duration::from_secs(1)).await;

    let opened = tokio::time::timeout(Duration::from_secs(1), open_rx.recv())
        .await
        .expect("expected on_connection_opened signal")
        .expect("open signal channel closed");
    assert!(!opened, "first open must not be marked reconnect");

    assert!(
        tokio::time::timeout(Duration::from_millis(150), sent_rx.recv())
            .await
            .is_err(),
        "auth-gated session must not auto-replay before auth"
    );

    actor
        .ask(WsSetAuthenticated {
            authenticated: true,
        })
        .await
        .unwrap();

    let first = tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
        .await
        .expect("expected replay after auth")
        .expect("writer channel closed");
    assert_eq!(frame_payload(&first), b"sub.private.a");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_gated_reconnect_requires_reauth_before_replay_and_emits_reconnect_signal() {
    let (sent_tx, mut sent_rx) = mpsc::unbounded_channel::<WsFrame>();
    let (open_tx, mut open_rx) = mpsc::unbounded_channel::<bool>();

    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: "ws://stub".to_string(),
        transport: StubTransport { sent_tx },
        reconnect_strategy: FastReconnect,
        handler: AuthGatedHandler::new(vec![b"sub.private.a".to_vec()], open_tx),
        ingress: ForwardAllIngress::default(),
        ping_strategy: ProtocolPingPong::new(Duration::from_secs(60), Duration::from_secs(60)),
        enable_ping: false,
        stale_threshold: Duration::from_secs(60),
        ws_buffers: WebSocketBufferConfig::default(),
        outbound_capacity: 32,
        circuit_breaker: None,
        latency_policy: None,
        payload_latency_sampling: None,
        registration: None,
        metrics: None,
    });

    actor.tell(WebSocketEvent::Connect).send().await.unwrap();
    wait_connected(&actor, Duration::from_secs(1)).await;
    assert_eq!(open_rx.recv().await, Some(false));

    actor
        .ask(WsSetAuthenticated {
            authenticated: true,
        })
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
        .await
        .expect("expected first replay")
        .expect("writer channel closed");
    assert_eq!(frame_payload(&first), b"sub.private.a");

    actor
        .tell(WebSocketEvent::Disconnect {
            reason: "test reconnect".to_string(),
            cause: WsDisconnectCause::EndpointRequested {
                reason: "test reconnect".to_string(),
            },
        })
        .send()
        .await
        .unwrap();

    wait_connected(&actor, Duration::from_secs(2)).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), open_rx.recv())
            .await
            .expect("expected reconnect open signal"),
        Some(true)
    );

    assert!(
        tokio::time::timeout(Duration::from_millis(150), sent_rx.recv())
            .await
            .is_err(),
        "reconnect must not replay before re-authentication"
    );

    actor
        .ask(WsSetAuthenticated {
            authenticated: true,
        })
        .await
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
        .await
        .expect("expected replay after re-auth")
        .expect("writer channel closed");
    assert_eq!(frame_payload(&second), b"sub.private.a");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_gated_explicit_replay_replays_desired_subscriptions_on_demand() {
    let (sent_tx, mut sent_rx) = mpsc::unbounded_channel::<WsFrame>();
    let (open_tx, mut open_rx) = mpsc::unbounded_channel::<bool>();

    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: "ws://stub".to_string(),
        transport: StubTransport { sent_tx },
        reconnect_strategy: NoReconnect,
        handler: AuthGatedHandler::new(vec![b"sub.private.a".to_vec()], open_tx),
        ingress: ForwardAllIngress::default(),
        ping_strategy: ProtocolPingPong::new(Duration::from_secs(60), Duration::from_secs(60)),
        enable_ping: false,
        stale_threshold: Duration::from_secs(60),
        ws_buffers: WebSocketBufferConfig::default(),
        outbound_capacity: 32,
        circuit_breaker: None,
        latency_policy: None,
        payload_latency_sampling: None,
        registration: None,
        metrics: None,
    });

    actor.tell(WebSocketEvent::Connect).send().await.unwrap();
    wait_connected(&actor, Duration::from_secs(1)).await;
    assert_eq!(open_rx.recv().await, Some(false));

    actor
        .ask(WsSetAuthenticated {
            authenticated: true,
        })
        .await
        .unwrap();

    let first = tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
        .await
        .expect("expected initial replay")
        .expect("writer channel closed");
    assert_eq!(frame_payload(&first), b"sub.private.a");

    actor.ask(WsReplaySubscriptions).await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
        .await
        .expect("expected explicit replay")
        .expect("writer channel closed");
    assert_eq!(frame_payload(&second), b"sub.private.a");
}
