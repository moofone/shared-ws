use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::Sink;
use kameo::Actor;
use shared_rate_limiter::RateLimiter;
use shared_ws::transport::{WsTransport, WsTransportConnectFuture};
use shared_ws::ws::{
    ForwardAllIngress, GetConnectionStatus, ProtocolPingPong, WebSocketActor, WebSocketActorArgs,
    WebSocketBufferConfig, WebSocketError, WebSocketEvent, WsConnectionStatus, WsFrame,
    WsReconnectStrategy, WsSubscriptionAction, WsSubscriptionManager, WsSubscriptionStatus,
    into_ws_message, message_bytes,
};
use tokio::sync::{Mutex, mpsc};

use examples_ws::coordinator::DelegatedOutcome;
use examples_ws::endpoints::deribit::{DeribitCoordinator, DeribitPublicHandler, DeribitSubscribe};

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

#[derive(Clone, Copy, Debug)]
enum WriterBehavior {
    Ok,
    Err(&'static str),
}

#[derive(Clone)]
struct TestTransport {
    behavior: WriterBehavior,
    sent_tx: mpsc::UnboundedSender<WsFrame>,
    inbound_rx: std::sync::Arc<Mutex<Option<mpsc::UnboundedReceiver<WsFrame>>>>,
}

impl WsTransport for TestTransport {
    type Reader = TestReader;
    type Writer = TestWriter;

    fn connect(
        &self,
        _url: String,
        _buffers: WebSocketBufferConfig,
    ) -> WsTransportConnectFuture<Self::Reader, Self::Writer> {
        let behavior = self.behavior;
        let sent_tx = self.sent_tx.clone();
        let inbound_rx = self.inbound_rx.clone();
        Box::pin(async move {
            let rx = inbound_rx
                .lock()
                .await
                .take()
                .expect("TestTransport only supports a single connect");
            Ok((TestReader { rx }, TestWriter { behavior, sent_tx }))
        })
    }
}

struct TestReader {
    rx: mpsc::UnboundedReceiver<WsFrame>,
}

impl futures_util::Stream for TestReader {
    type Item = Result<WsFrame, WebSocketError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.rx).poll_recv(cx) {
            Poll::Ready(Some(frame)) => Poll::Ready(Some(Ok(frame))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

struct TestWriter {
    behavior: WriterBehavior,
    sent_tx: mpsc::UnboundedSender<WsFrame>,
}

impl Sink<WsFrame> for TestWriter {
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
                context: "test_write",
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
    DeribitPublicHandler<NoSubscriptions>,
    NoReconnect,
    ProtocolPingPong,
    ForwardAllIngress<()>,
    TestTransport,
>;
type TestWsActorRef = kameo::prelude::ActorRef<TestWsActor>;

fn spawn_actor(
    behavior: WriterBehavior,
) -> (
    TestWsActorRef,
    mpsc::UnboundedReceiver<WsFrame>,
    mpsc::UnboundedSender<WsFrame>,
) {
    let (sent_tx, sent_rx) = mpsc::unbounded_channel::<WsFrame>();
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<WsFrame>();
    let transport = TestTransport {
        behavior,
        sent_tx: sent_tx.clone(),
        inbound_rx: std::sync::Arc::new(Mutex::new(Some(inbound_rx))),
    };

    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: "ws://test".to_string(),
        transport,
        reconnect_strategy: NoReconnect,
        handler: DeribitPublicHandler::new(NoSubscriptions),
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

    (actor, sent_rx, inbound_tx)
}

async fn wait_connected<A>(actor: &kameo::prelude::ActorRef<A>, timeout: Duration)
where
    A: Actor,
    A: kameo::prelude::Message<
            GetConnectionStatus,
            Reply = Result<WsConnectionStatus, shared_ws::ws::WebSocketError>,
        >,
{
    let deadline = Instant::now() + timeout;
    loop {
        let status = actor.ask(GetConnectionStatus).await.unwrap();
        if matches!(status, WsConnectionStatus::Connected) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for Connected status (last={status:?})");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegated_subscribe_happy_path_confirmed() {
    let cfg = shared_rate_limiter::Config::fixed_window(10, Duration::from_millis(200)).unwrap();
    let limiter = RateLimiter::new(cfg);
    let coordinator = DeribitCoordinator::new(limiter, 0);

    let (actor, mut sent_rx, inbound_tx) = spawn_actor(WriterBehavior::Ok);
    actor.tell(WebSocketEvent::Connect).send().await.unwrap();
    wait_connected(&actor, Duration::from_secs(1)).await;

    let responder = tokio::spawn(async move {
        let outbound = sent_rx.recv().await.expect("expected outbound");
        let bytes = message_bytes(&outbound).unwrap();
        let id = parse_jsonrpc_id(bytes).expect("id");
        let resp = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"ok\":true}}}}",
            id
        );
        let _ = inbound_tx.send(into_ws_message(resp));
    });

    let outcome = coordinator
        .delegated_subscribe(
            &actor,
            DeribitSubscribe {
                channels: vec!["trades.BTC-PERPETUAL.raw".to_string()],
            },
            Instant::now() + Duration::from_secs(1),
        )
        .await;

    responder.await.unwrap();

    match outcome {
        DelegatedOutcome::Confirmed(ok) => assert!(ok.confirmed),
        other => panic!("expected Confirmed, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegated_subscribe_denied_by_rate_limiter() {
    let cfg = shared_rate_limiter::Config::fixed_window(0, Duration::from_millis(200)).unwrap();
    let limiter = RateLimiter::new(cfg);
    let coordinator = DeribitCoordinator::new(limiter, 0);

    let (actor, mut sent_rx, _inbound_tx) = spawn_actor(WriterBehavior::Ok);
    actor.tell(WebSocketEvent::Connect).send().await.unwrap();
    wait_connected(&actor, Duration::from_secs(1)).await;

    let outcome = coordinator
        .delegated_subscribe(
            &actor,
            DeribitSubscribe {
                channels: vec!["trades.BTC-PERPETUAL.raw".to_string()],
            },
            Instant::now() + Duration::from_millis(50),
        )
        .await;

    assert!(matches!(outcome, DelegatedOutcome::Denied { .. }));
    assert!(
        tokio::time::timeout(Duration::from_millis(25), sent_rx.recv())
            .await
            .is_err(),
        "denied should not send anything"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegated_subscribe_unconfirmed_times_out() {
    let cfg = shared_rate_limiter::Config::fixed_window(10, Duration::from_millis(200)).unwrap();
    let limiter = RateLimiter::new(cfg);
    let coordinator = DeribitCoordinator::new(limiter, 0);

    let (actor, _sent_rx, _inbound_tx) = spawn_actor(WriterBehavior::Ok);
    actor.tell(WebSocketEvent::Connect).send().await.unwrap();
    wait_connected(&actor, Duration::from_secs(1)).await;

    let outcome = coordinator
        .delegated_subscribe(
            &actor,
            DeribitSubscribe {
                channels: vec!["trades.BTC-PERPETUAL.raw".to_string()],
            },
            Instant::now() + Duration::from_millis(30),
        )
        .await;

    assert!(matches!(outcome, DelegatedOutcome::Unconfirmed(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegated_subscribe_not_delivered_refunds() {
    let cfg = shared_rate_limiter::Config::fixed_window(1, Duration::from_millis(200)).unwrap();
    let limiter = RateLimiter::new(cfg);
    let coordinator = DeribitCoordinator::new(limiter, 0);

    let (actor1, _sent_rx1, _inbound_tx1) = spawn_actor(WriterBehavior::Err("boom"));
    actor1.tell(WebSocketEvent::Connect).send().await.unwrap();
    wait_connected(&actor1, Duration::from_secs(1)).await;

    let o1 = coordinator
        .delegated_subscribe(
            &actor1,
            DeribitSubscribe {
                channels: vec!["trades.BTC-PERPETUAL.raw".to_string()],
            },
            Instant::now() + Duration::from_secs(1),
        )
        .await;
    assert!(matches!(o1, DelegatedOutcome::NotDelivered(_)));

    let (actor2, mut sent_rx2, _inbound_tx2) = spawn_actor(WriterBehavior::Ok);
    actor2.tell(WebSocketEvent::Connect).send().await.unwrap();
    wait_connected(&actor2, Duration::from_secs(1)).await;

    let o2 = coordinator
        .delegated_subscribe(
            &actor2,
            DeribitSubscribe {
                channels: vec!["trades.BTC-PERPETUAL.raw".to_string()],
            },
            Instant::now() + Duration::from_millis(30),
        )
        .await;

    assert!(!matches!(o2, DelegatedOutcome::Denied { .. }));
    tokio::time::timeout(Duration::from_millis(200), sent_rx2.recv())
        .await
        .expect("expected send after refund")
        .expect("expected outbound frame");
}

fn parse_jsonrpc_id(data: &[u8]) -> Option<u64> {
    let mut i = 0usize;
    while i + 4 < data.len() {
        if data[i] == b'"'
            && data.get(i + 1) == Some(&b'i')
            && data.get(i + 2) == Some(&b'd')
            && data.get(i + 3) == Some(&b'"')
        {
            i += 4;
            while i < data.len() && matches!(data[i], b' ' | b'\n' | b'\r' | b'\t') {
                i += 1;
            }
            if i >= data.len() || data[i] != b':' {
                continue;
            }
            i += 1;
            while i < data.len() && matches!(data[i], b' ' | b'\n' | b'\r' | b'\t') {
                i += 1;
            }
            let start = i;
            while i < data.len() && data[i].is_ascii_digit() {
                i += 1;
            }
            if start == i {
                return None;
            }
            return std::str::from_utf8(&data[start..i])
                .ok()?
                .parse::<u64>()
                .ok();
        }
        i += 1;
    }
    None
}
