use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::Sink;
use futures_util::stream;
use kameo::Actor;
use kameo::error::SendError;
use shared_ws::transport::{WsTransport, WsTransportConnectFuture};
use shared_ws::ws::{
    ForwardAllIngress, ProtocolPingPong, WebSocketActor, WebSocketActorArgs, WebSocketBufferConfig,
    WebSocketError, WebSocketEvent, WsConfirmMode, WsDelegatedError, WsDelegatedRequest,
    WsDisconnectAction, WsDisconnectCause, WsEndpointHandler, WsErrorAction, WsFrame,
    WsMessageAction, WsParseOutcome, WsReconnectStrategy, WsSubscriptionAction,
    WsSubscriptionManager, WsSubscriptionStatus,
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

#[derive(Clone, Copy, Debug)]
enum WriterBehavior {
    Ok,
    Err(&'static str),
}

#[derive(Clone)]
struct StubTransport {
    behavior: WriterBehavior,
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
        let behavior = self.behavior;
        let tx = self.sent_tx.clone();
        Box::pin(async move {
            Ok((
                stream::pending(),
                StubWriter {
                    behavior,
                    sent_tx: tx,
                },
            ))
        })
    }
}

struct StubWriter {
    behavior: WriterBehavior,
    sent_tx: mpsc::UnboundedSender<WsFrame>,
}

impl Sink<WsFrame> for StubWriter {
    type Error = WebSocketError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: WsFrame) -> Result<(), Self::Error> {
        let this = self.get_mut();
        match this.behavior {
            WriterBehavior::Ok => {
                let _ = this.sent_tx.send(item);
                Ok(())
            }
            WriterBehavior::Err(msg) => Err(WebSocketError::TransportError {
                context: "stub_write",
                error: msg.to_string(),
            }),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

type TestWsActor = WebSocketActor<
    NoopHandler,
    NoReconnect,
    ProtocolPingPong,
    ForwardAllIngress<()>,
    StubTransport,
>;
type TestWsActorRef = kameo::prelude::ActorRef<TestWsActor>;

fn spawn_actor(behavior: WriterBehavior) -> (TestWsActorRef, mpsc::UnboundedReceiver<WsFrame>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let transport = StubTransport {
        behavior,
        sent_tx: tx,
    };

    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: "ws://stub".to_string(),
        transport,
        reconnect_strategy: NoReconnect,
        handler: NoopHandler::new(),
        ping_strategy: ProtocolPingPong::new(Duration::from_secs(60), Duration::from_secs(60)),
        ingress: ForwardAllIngress::default(),
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

    (actor, rx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegated_not_delivered_when_writer_fails() {
    let (actor, _sent_rx) = spawn_actor(WriterBehavior::Err("boom"));

    actor.tell(WebSocketEvent::Connect).send().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let res = actor
        .ask(WsDelegatedRequest {
            request_id: 1,
            fingerprint: 111,
            frame: WsFrame::text_static("hi"),
            confirm_deadline: Instant::now() + Duration::from_secs(5),
            confirm_mode: WsConfirmMode::Sent,
        })
        .await;

    let err = res.expect_err("expected delegated request to fail");
    match err {
        SendError::HandlerError(WsDelegatedError::NotDelivered { request_id: 1, .. }) => {}
        other => panic!("expected NotDelivered handler error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegated_unconfirmed_on_timeout_and_dedups_waiters() {
    let (actor, mut sent_rx) = spawn_actor(WriterBehavior::Ok);

    actor.tell(WebSocketEvent::Connect).send().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let deadline = Instant::now() + Duration::from_millis(40);
    let req = WsDelegatedRequest {
        request_id: 42,
        fingerprint: 999,
        frame: WsFrame::text_static("req"),
        confirm_deadline: deadline,
        confirm_mode: WsConfirmMode::Confirmed,
    };

    let a1 = actor.clone();
    let a2 = actor.clone();
    let req1 = req.clone();
    let t1 = tokio::spawn(async move { a1.ask(req1).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    let t2 = tokio::spawn(async move { a2.ask(req).await });

    let r1 = t1.await.unwrap().expect_err("expected timeout");
    let r2 = t2.await.unwrap().expect_err("expected timeout");

    match r1 {
        SendError::HandlerError(WsDelegatedError::Unconfirmed { request_id: 42, .. }) => {}
        other => panic!("expected Unconfirmed handler error, got {other:?}"),
    }
    match r2 {
        SendError::HandlerError(WsDelegatedError::Unconfirmed { request_id: 42, .. }) => {}
        other => panic!("expected Unconfirmed handler error, got {other:?}"),
    }

    // Dedup: only one outbound send should occur for the in-flight request.
    let first = tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
        .await
        .expect("timed out waiting for outbound frame")
        .expect("expected outbound frame");
    assert_eq!(first, WsFrame::text_static("req"));
    assert!(
        tokio::time::timeout(Duration::from_millis(25), sent_rx.recv())
            .await
            .is_err(),
        "expected no second send"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegated_sent_joiner_after_send_ok_completes_immediately_even_if_confirmed_pending() {
    let (actor, mut sent_rx) = spawn_actor(WriterBehavior::Ok);

    actor.tell(WebSocketEvent::Connect).send().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let deadline = Instant::now() + Duration::from_millis(80);
    let confirmed_req = WsDelegatedRequest {
        request_id: 7,
        fingerprint: 777,
        frame: WsFrame::text_static("req"),
        confirm_deadline: deadline,
        confirm_mode: WsConfirmMode::Confirmed,
    };

    let confirmed_task = tokio::spawn({
        let a = actor.clone();
        async move { a.ask(confirmed_req).await }
    });

    // Wait until the single deduped send has happened and the actor has observed `sent_ok`.
    let first = tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
        .await
        .expect("timed out waiting for outbound frame")
        .expect("expected outbound frame");
    assert_eq!(first, WsFrame::text_static("req"));

    // Now join with `Sent` mode after the send is already observed.
    let sent_res = tokio::time::timeout(
        Duration::from_millis(30),
        actor.ask(WsDelegatedRequest {
            request_id: 7,
            fingerprint: 777,
            frame: WsFrame::text_static("req"),
            confirm_deadline: deadline,
            confirm_mode: WsConfirmMode::Sent,
        }),
    )
    .await
    .expect("sent joiner should complete immediately")
    .expect("expected ok");
    assert_eq!(sent_res.request_id, 7);
    assert!(!sent_res.confirmed);

    // Confirmed-mode original should still time out (no matcher in this sprint test).
    let r1 = confirmed_task.await.unwrap().expect_err("expected timeout");
    match r1 {
        SendError::HandlerError(WsDelegatedError::Unconfirmed { request_id: 7, .. }) => {}
        other => panic!("expected Unconfirmed handler error, got {other:?}"),
    }

    // Still only one send should occur.
    assert!(
        tokio::time::timeout(Duration::from_millis(25), sent_rx.recv())
            .await
            .is_err(),
        "expected no second send"
    );
}
