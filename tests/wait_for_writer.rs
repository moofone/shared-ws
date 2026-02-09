use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::Sink;
use futures_util::stream;
use kameo::Actor;
use kameo::error::SendError;
use shared_ws::transport::{WsTransport, WsTransportConnectFuture};
use shared_ws::ws::{
    ExponentialBackoffReconnect, ForwardAllIngress, GetConnectionStatus, ProtocolPingPong,
    WaitForWriter, WebSocketActor, WebSocketActorArgs, WebSocketBufferConfig, WebSocketError,
    WebSocketEvent, WsConnectionStatus, WsDisconnectAction, WsDisconnectCause, WsEndpointHandler,
    WsErrorAction, WsFrame, WsMessageAction, WsParseOutcome, WsReconnectStrategy,
    WsSubscriptionAction, WsSubscriptionManager, WsSubscriptionStatus,
};

#[derive(Clone)]
struct DelayedTransport {
    delay: Duration,
}

impl WsTransport for DelayedTransport {
    type Reader = stream::Pending<Result<WsFrame, WebSocketError>>;
    type Writer = StubWriter;

    fn connect(
        &self,
        _url: String,
        _buffers: WebSocketBufferConfig,
    ) -> WsTransportConnectFuture<Self::Reader, Self::Writer> {
        let delay = self.delay;
        Box::pin(async move {
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
async fn wait_for_writer_completes_after_connect() {
    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: "ws://delayed".to_string(),
        transport: DelayedTransport {
            delay: Duration::from_millis(50),
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

    let wait_task = {
        let actor = actor.clone();
        tokio::spawn(async move {
            actor
                .ask(WaitForWriter {
                    timeout: Duration::from_secs(1),
                })
                .await
        })
    };

    actor.tell(WebSocketEvent::Connect).send().await.unwrap();

    let res = wait_task.await.unwrap();
    assert!(res.is_ok(), "WaitForWriter should succeed, got {res:?}");
    wait_connected(&actor, Duration::from_secs(1)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_for_writer_times_out_without_connect() {
    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: "ws://never-connect".to_string(),
        transport: DelayedTransport {
            delay: Duration::from_secs(365 * 24 * 60 * 60),
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

    let res = actor
        .ask(WaitForWriter {
            timeout: Duration::from_millis(50),
        })
        .await;

    let err = res.expect_err("expected WaitForWriter to time out");
    assert!(matches!(
        err,
        SendError::HandlerError(WebSocketError::Timeout { .. })
    ));
}
