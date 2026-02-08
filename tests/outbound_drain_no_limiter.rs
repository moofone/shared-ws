use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::Sink;
use futures_util::stream;
use kameo::Actor;
use shared_ws::transport::{WsTransport, WsTransportConnectFuture};
use shared_ws::ws::{
    ForwardAllIngress, GetConnectionStatus, ProtocolPingPong, WebSocketActor, WebSocketActorArgs,
    WebSocketBufferConfig, WebSocketError, WebSocketEvent, WsDisconnectAction, WsDisconnectCause,
    WsEndpointHandler, WsErrorAction, WsFrame, WsMessageAction, WsParseOutcome,
    WsReconnectStrategy, WsSubscriptionAction, WsSubscriptionManager, WsSubscriptionStatus,
    WsTlsConfig,
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

#[derive(Debug, Default, Clone)]
struct NoSubscriptions;

impl WsSubscriptionManager for NoSubscriptions {
    type SubscriptionMessage = ();

    fn initial_subscriptions(&mut self) -> Vec<Self::SubscriptionMessage> {
        Vec::new()
    }

    fn update_subscriptions(
        &mut self,
        _action: WsSubscriptionAction<Self::SubscriptionMessage>,
    ) -> Result<Vec<Self::SubscriptionMessage>, String> {
        Ok(Vec::new())
    }

    fn serialize_subscription(&mut self, _msg: &Self::SubscriptionMessage) -> Vec<u8> {
        Vec::new()
    }

    fn handle_subscription_response(&mut self, _data: &[u8]) -> WsSubscriptionStatus {
        WsSubscriptionStatus::NotSubscriptionResponse
    }
}

#[derive(Clone)]
struct NoopHandler {
    subs: NoSubscriptions,
}

impl NoopHandler {
    fn new() -> Self {
        Self {
            subs: NoSubscriptions,
        }
    }
}

impl WsEndpointHandler for NoopHandler {
    type Message = ();
    type Error = std::io::Error;
    type Subscription = NoSubscriptions;

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

    fn reset_state(&mut self) {}

    fn classify_disconnect(&self, _cause: &WsDisconnectCause) -> WsDisconnectAction {
        WsDisconnectAction::Abort
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
        _tls: WsTlsConfig,
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
    let deadline = Instant::now() + timeout;
    loop {
        let status = actor.ask(GetConnectionStatus).await.unwrap();
        if matches!(status, shared_ws::ws::WsConnectionStatus::Connected) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for Connected status (last={status:?})");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outbound_drain_sends_promptly_without_internal_retry_scheduling() {
    let (sent_tx, mut sent_rx) = mpsc::unbounded_channel::<WsFrame>();
    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: "ws://stub".to_string(),
        tls: WsTlsConfig::default(),
        transport: StubTransport { sent_tx },
        reconnect_strategy: NoReconnect,
        handler: NoopHandler::new(),
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

    actor
        .tell(WebSocketEvent::SendMessage(WsFrame::text_static("m1")))
        .send()
        .await
        .unwrap();
    actor
        .tell(WebSocketEvent::SendMessage(WsFrame::text_static("m2")))
        .send()
        .await
        .unwrap();

    let first = tokio::time::timeout(Duration::from_millis(200), sent_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let second = tokio::time::timeout(Duration::from_millis(200), sent_rx.recv())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(first, WsFrame::text_static("m1"));
    assert_eq!(second, WsFrame::text_static("m2"));
}
