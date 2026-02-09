use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::Sink;
use futures_util::stream;
use shared_ws::transport::{WsTransport, WsTransportConnectFuture};
use shared_ws::ws::{
    ExponentialBackoffReconnect, ForwardAllIngress, GetConnectionStatus, ProtocolPingPong,
    WebSocketActor, WebSocketActorArgs, WebSocketBufferConfig, WebSocketError, WebSocketEvent,
    WsConnectionStatus, WsDisconnectAction, WsDisconnectCause, WsEndpointHandler, WsErrorAction,
    WsFrame, WsMessageAction, WsParseOutcome, WsReconnectStrategy, WsSubscriptionAction,
    WsSubscriptionManager, WsSubscriptionStatus,
};

#[derive(Clone)]
struct CountingTransport {
    connects: Arc<AtomicUsize>,
    delay: Duration,
}

impl WsTransport for CountingTransport {
    type Reader = stream::Pending<Result<WsFrame, WebSocketError>>;
    type Writer = StubWriter;

    fn connect(
        &self,
        _url: String,
        _buffers: WebSocketBufferConfig,
    ) -> WsTransportConnectFuture<Self::Reader, Self::Writer> {
        let connects = self.connects.clone();
        let delay = self.delay;
        Box::pin(async move {
            connects.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            Ok((stream::pending(), StubWriter))
        })
    }
}

#[derive(Clone, Copy)]
struct StubWriter;

impl Sink<WsFrame> for StubWriter {
    type Error = WebSocketError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, _item: WsFrame) -> Result<(), Self::Error> {
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[derive(Clone, Debug, Default)]
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

#[derive(Clone, Debug, Default)]
struct Handler {
    subs: NoSubscriptions,
}

impl WsEndpointHandler for Handler {
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
        if status == WsConnectionStatus::Connected {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for Connected status (last={status:?})");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_is_single_flight_and_idempotent() {
    let connects = Arc::new(AtomicUsize::new(0));
    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: "ws://counting".to_string(),
        transport: CountingTransport {
            connects: connects.clone(),
            delay: Duration::from_millis(100),
        },
        reconnect_strategy: ExponentialBackoffReconnect::default(),
        handler: Handler::default(),
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

    for _ in 0..50 {
        actor.tell(WebSocketEvent::Connect).send().await.unwrap();
    }

    wait_connected(&actor, Duration::from_secs(1)).await;
    assert_eq!(
        connects.load(Ordering::SeqCst),
        1,
        "connect() should only be invoked once while a handshake is in flight"
    );
}
