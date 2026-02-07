//! High-performance websocket actor.
//!
//! The websocket IO loop runs outside kameo for maximum throughput; the actor owns
//! connection state/policies and receives frames via messages.

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use sonic_rs::Value;
use tokio::{
    sync::{Notify, watch},
    task::JoinHandle,
    time::MissedTickBehavior,
};
use tracing::{info, warn};

use super::WsMetricsHook;
use super::health::WsHealthMonitor;
use super::ping::{WsPingPongStrategy, WsPongResult};
use super::rate_limit::{WsCircuitBreaker, WsRateLimiter, jitter_delay};
use super::types::{
    ForwardAllIngress, WebSocketBufferConfig, WebSocketError, WebSocketResult, WsActorRegistration,
    WsBufferPool, WsConnectionStats, WsConnectionStatus, WsDisconnectAction, WsDisconnectCause,
    WsErrorAction, WsEndpointHandler, WsIngress, WsIngressAction, WsLatencyPercentile,
    WsLatencyPolicy, WsMessage, WsMessageAction, WsParseOutcome, WsReconnectStrategy,
    WsSubscriptionAction, WsSubscriptionManager, WsTlsConfig, WsSubscriptionStatus, into_ws_message,
    message_bytes,
};
use super::writer::{
    WriterWrite, WsWriterActor, spawn_writer_supervisor, spawn_writer_supervised_with,
};
use crate::transport::WsTransport;
use crate::transport::tungstenite::TungsteniteTransport;
use crate::supervision::TypedSupervisor;
use kameo::error::RegistryError;
use kameo::prelude::{Actor, ActorRef, Context, Message as KameoMessage, WeakActorRef};

#[derive(Clone, Copy, Debug)]
struct LatencyBreach {
    percentile: WsLatencyPercentile,
    observed_us: u64,
    threshold_us: u64,
    samples: u64,
}

/// Arguments passed when constructing a websocket actor instance.
pub struct WebSocketActorArgs<E, R, P, I = ForwardAllIngress, T = TungsteniteTransport>
where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: WsPingPongStrategy,
    I: WsIngress<Message = E::Message>,
    T: WsTransport,
{
    pub url: String,
    pub tls: WsTlsConfig,
    pub transport: T,
    pub reconnect_strategy: R,
    pub handler: E,
    pub ping_strategy: P,
    pub ingress: I,
    pub enable_ping: bool,
    pub stale_threshold: Duration,
    pub ws_buffers: WebSocketBufferConfig,
    pub outbound_capacity: usize,
    pub rate_limiter: Option<WsRateLimiter>,
    pub circuit_breaker: Option<WsCircuitBreaker>,
    pub latency_policy: Option<WsLatencyPolicy>,
    pub registration: Option<WsActorRegistration>,
    pub metrics: Option<WsMetricsHook>,
}

/// Skeleton websocket actor that will be fleshed out in later sprints.
pub struct WebSocketActor<E, R, P, I = ForwardAllIngress, T = TungsteniteTransport>
where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: WsPingPongStrategy,
    I: WsIngress<Message = E::Message>,
    T: WsTransport,
{
    pub url: String,
    pub tls: WsTlsConfig,
    pub transport: T,
    pub health: WsHealthMonitor,
    pub reconnect: R,
    pub handler: E,
    pub ping: P,
    pub ingress: Option<I>,
    pub enable_ping: bool,
    pub actor_ref: ActorRef<Self>,
    pub buffers: WsBufferPool,
    pub ws_buffers: WebSocketBufferConfig,
    pub reader_task: Option<JoinHandle<I>>,
    pub ping_task: Option<JoinHandle<()>>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    pub writer_ref: Option<ActorRef<WsWriterActor<T::Writer>>>,
    writer_supervisor_ref: Option<ActorRef<TypedSupervisor<WsWriterActor<T::Writer>>>>,
    writer_ready: Arc<Notify>,
    writer_ready_flag: Arc<AtomicBool>,
    pub outbound_capacity: usize,
    pub rate_limiter: Option<WsRateLimiter>,
    pub circuit_breaker: Option<WsCircuitBreaker>,
    pub latency_policy: Option<WsLatencyPolicy>,
    pub latency_breach_streak: u32,
    pub status: WsConnectionStatus,
    pub registration: Option<WsActorRegistration>,
    pub reconnect_attempt: u64,
    pub metrics: Option<WsMetricsHook>,
    pending_outbound: VecDeque<WsMessage>,
    inbound_bytes_total: u64,
    outbound_bytes_total: u64,
    last_inbound_payload_len: Option<usize>,
    last_outbound_payload_len: Option<usize>,
    _phantom: PhantomData<P>,
}

impl<E, R, P, I, T> Actor for WebSocketActor<E, R, P, I, T>
where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: WsPingPongStrategy,
    I: WsIngress<Message = E::Message>,
    T: WsTransport,
{
    type Args = WebSocketActorArgs<E, R, P, I, T>;
    type Error = WebSocketError;

    fn name() -> &'static str {
        "WebSocketActor"
    }

    async fn on_start(args: Self::Args, ctx: ActorRef<Self>) -> WebSocketResult<Self> {
        let WebSocketActorArgs {
            url,
            tls,
            transport,
            reconnect_strategy,
            handler,
            ping_strategy,
            ingress,
            enable_ping,
            stale_threshold,
            ws_buffers,
            outbound_capacity,
            rate_limiter,
            circuit_breaker,
            latency_policy,
            registration,
            metrics,
        } = args;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let actor_ref = ctx.clone();
        let buffers = WsBufferPool::new(ws_buffers);

        if let Some(registration) = registration.as_ref() {
            let actor_name = registration.name.clone();
            // We only need local discovery for WS pools; distributed registration requires
            // implementing `ActorRegistration`, which we intentionally avoid here.
            let register_result = ctx.register_local(actor_name.clone());

            match register_result {
                Ok(_) => {
                    tracing::debug!(
                        actor = %actor_name,
                        "registered websocket actor in local registry"
                    )
                }
                Err(RegistryError::NameAlreadyRegistered) => {
                    tracing::debug!(actor = %actor_name, "websocket actor already registered in local registry")
                }
                Err(err) => {
                    warn!(actor = %actor_name, error = %err, "failed to register websocket actor")
                }
            }
        }

        Ok(Self {
            url,
            tls,
            transport,
            health: WsHealthMonitor::new(stale_threshold),
            reconnect: reconnect_strategy,
            handler,
            ping: ping_strategy,
            ingress: Some(ingress),
            enable_ping,
            actor_ref,
            buffers,
            ws_buffers,
            reader_task: None,
            ping_task: None,
            shutdown_tx,
            shutdown_rx,
            writer_ref: None,
            writer_supervisor_ref: None,
            writer_ready: Arc::new(Notify::new()),
            writer_ready_flag: Arc::new(AtomicBool::new(false)),
            outbound_capacity,
            rate_limiter,
            circuit_breaker,
            latency_policy,
            latency_breach_streak: 0,
            status: WsConnectionStatus::Connecting,
            registration,
            reconnect_attempt: 0,
            metrics,
            pending_outbound: VecDeque::with_capacity(outbound_capacity),
            inbound_bytes_total: 0,
            outbound_bytes_total: 0,
            last_inbound_payload_len: None,
            last_outbound_payload_len: None,
            _phantom: PhantomData,
        })
    }

    async fn on_stop(
        &mut self,
        _ctx: WeakActorRef<Self>,
        _reason: kameo::error::ActorStopReason,
    ) -> WebSocketResult<()> {
        self.stop_all_tasks().await;
        Ok(())
    }

    fn on_panic(
        &mut self,
        _actor_ref: kameo::actor::WeakActorRef<Self>,
        err: kameo::prelude::PanicError,
    ) -> impl std::future::Future<
        Output = Result<std::ops::ControlFlow<kameo::prelude::ActorStopReason>, Self::Error>,
    > + Send {
        async move {
            tracing::error!(error = ?err, "WebSocketActor panicked");
            Ok(std::ops::ControlFlow::Break(
                kameo::prelude::ActorStopReason::Panicked(err),
            ))
        }
    }
}

impl<E, R, P, I, T> KameoMessage<WebSocketEvent> for WebSocketActor<E, R, P, I, T>
where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: WsPingPongStrategy,
    I: WsIngress<Message = E::Message>,
    T: WsTransport,
{
    type Reply = WebSocketResult<()>;

    async fn handle(
        &mut self,
        event: WebSocketEvent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match event {
            WebSocketEvent::Connect => {
                self.status = WsConnectionStatus::Connecting;
                self.handle_connect().await?;
            }
            WebSocketEvent::Disconnect { reason, cause } => {
                self.status = WsConnectionStatus::Disconnected;
                self.handle_disconnect(reason, cause).await?;
            }
            WebSocketEvent::Inbound(message) => {
                self.process_inbound(message).await?;
            }
            WebSocketEvent::InboundActivity {
                frames,
                bytes,
                last_payload_len,
            } => {
                self.health.record_inbound_frames(frames);
                self.inbound_bytes_total = self.inbound_bytes_total.saturating_add(bytes);
                if let Some(len) = last_payload_len {
                    self.last_inbound_payload_len = Some(len);
                }
            }
            WebSocketEvent::ServerError {
                code,
                message,
                data,
            } => {
                self.health.record_server_error(code, &message);
                self.handle_server_error(code, message, data).await?;
            }
            WebSocketEvent::InternalError { context, error } => {
                self.health.record_internal_error(&context, &error);
                let reason = format!("internal error ({context}): {error}");
                let cause = WsDisconnectCause::InternalError { context, error };
                self.handle_disconnect(reason, cause).await?;
            }
            WebSocketEvent::SendPing => {
                self.emit_ping().await?;
            }
            WebSocketEvent::CheckStale => {
                self.check_stale().await?;
            }
            WebSocketEvent::SendMessage(message) => {
                if let Err(err) = self.enqueue_message(message).await {
                    let reason = format!("outbound send failed: {err}");
                    let cause = WsDisconnectCause::InternalError {
                        context: "outbound_send".to_string(),
                        error: err.to_string(),
                    };
                    self.handle_disconnect(reason, cause).await?;
                }
            }
            WebSocketEvent::SendBatch(messages) => {
                for msg in messages {
                    if let Err(err) = self.enqueue_message(msg).await {
                        let reason = format!("outbound send failed: {err}");
                        let cause = WsDisconnectCause::InternalError {
                            context: "outbound_send".to_string(),
                            error: err.to_string(),
                        };
                        self.handle_disconnect(reason, cause).await?;
                        break;
                    }
                }
            }
            WebSocketEvent::ResetState => self.handler.reset_state(),
            WebSocketEvent::UpdateConnectionStatus(status) => self.status = status,
        }
        Ok(())
    }
}

pub(crate) struct ConnectionEstablished<TR: WsTransport>(
    pub(crate) TR::Reader,
    pub(crate) TR::Writer,
);

#[derive(Debug)]
pub struct IngressEmit<M: Send + 'static> {
    pub message: M,
}

pub(crate) struct ConnectionFailed {
    pub(crate) error: String,
    pub(crate) status: Option<u16>,
}

impl<E, R, P, I, T> KameoMessage<ConnectionEstablished<T>> for WebSocketActor<E, R, P, I, T>
where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: WsPingPongStrategy,
    I: WsIngress<Message = E::Message>,
    T: WsTransport,
{
    type Reply = WebSocketResult<()>;

    async fn handle(
        &mut self,
        msg: ConnectionEstablished<T>,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.status = WsConnectionStatus::Connected;
        self.on_connection_established(msg.0, msg.1).await
    }
}

impl<E, R, P, I, T> KameoMessage<ConnectionFailed> for WebSocketActor<E, R, P, I, T>
where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: WsPingPongStrategy,
    I: WsIngress<Message = E::Message>,
    T: WsTransport,
{
    type Reply = WebSocketResult<()>;

    async fn handle(
        &mut self,
        msg: ConnectionFailed,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.status = WsConnectionStatus::Disconnected;
        self.handle_connection_failed(msg.error, msg.status).await
    }
}

impl<E, R, P, I, T> KameoMessage<IngressEmit<E::Message>> for WebSocketActor<E, R, P, I, T>
where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: WsPingPongStrategy,
    I: WsIngress<Message = E::Message>,
    T: WsTransport,
{
    type Reply = WebSocketResult<()>;

    async fn handle(
        &mut self,
        msg: IngressEmit<E::Message>,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_message_action(WsMessageAction::Process(msg.message))
            .await
    }
}

pub struct GetConnectionStats;

impl<E, R, P, I, T> KameoMessage<GetConnectionStats> for WebSocketActor<E, R, P, I, T>
where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: WsPingPongStrategy,
    I: WsIngress<Message = E::Message>,
    T: WsTransport,
{
    type Reply = WebSocketResult<WsConnectionStats>;

    async fn handle(
        &mut self,
        _message: GetConnectionStats,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.health.get_stats())
    }
}

impl<E, R, P, I, T> WebSocketActor<E, R, P, I, T>
where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: WsPingPongStrategy,
    I: WsIngress<Message = E::Message>,
    T: WsTransport,
{
    async fn stop_all_tasks(&mut self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(ingress) = Self::await_reader(&mut self.reader_task).await {
            self.ingress = Some(ingress);
        }
        Self::await_task(&mut self.ping_task).await;
        self.teardown_writer().await;
        self.reset_channels();
    }

    async fn stop_io_tasks_only(&mut self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(ingress) = Self::await_reader(&mut self.reader_task).await {
            self.ingress = Some(ingress);
        }
        Self::await_task(&mut self.ping_task).await;
        self.teardown_writer().await;
        self.reset_shutdown_channel();
    }

    async fn await_task(handle: &mut Option<JoinHandle<()>>) {
        if let Some(handle) = handle.take() {
            if let Err(err) = handle.await {
                warn!("task terminated with error: {err}");
            }
        }
    }

    async fn await_reader(handle: &mut Option<JoinHandle<I>>) -> Option<I> {
        let Some(handle) = handle.take() else {
            return None;
        };
        match handle.await {
            Ok(ingress) => Some(ingress),
            Err(err) => {
                warn!("reader task terminated with error: {err}");
                None
            }
        }
    }

    fn reset_channels(&mut self) {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = shutdown_tx;
        self.shutdown_rx = shutdown_rx;
        self.writer_ref = None;
        self.pending_outbound.clear();
        self.writer_ready = Arc::new(Notify::new());
        self.writer_ready_flag.store(false, Ordering::Release);
    }

    fn reset_shutdown_channel(&mut self) {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = shutdown_tx;
        self.shutdown_rx = shutdown_rx;
    }

    async fn teardown_writer(&mut self) {
        let writer = self.writer_ref.take();

        if let (Some(writer), Some(supervisor)) =
            (&writer, self.writer_supervisor_ref.as_ref())
        {
            let _ = writer.stop_gracefully().await;
            writer.wait_for_shutdown().await;
            writer.unlink(supervisor).await;
        }
    }

    async fn handle_connect(&mut self) -> WebSocketResult<()> {
        if let Some(cb) = self.circuit_breaker.as_mut() {
            if !cb.can_proceed() {
                if let Some(delay) = cb.time_until_retry() {
                    let actor_ref = self.actor_ref.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = actor_ref.tell(WebSocketEvent::Connect).send().await;
                    });
                }
                return Ok(());
            }
        }

        let self_ref = self.actor_ref.clone();
        let url = self.url.clone();
        let buffers = self.ws_buffers;
        let tls = self.tls;
        let transport = self.transport.clone();

        tokio::spawn(async move {
            match transport.connect(url, buffers, tls).await {
                Ok((reader, writer)) => {
                    let _ = self_ref
                        .tell(ConnectionEstablished::<T>(reader, writer))
                        .send()
                        .await;
                }
                Err(err) => {
                    let _ = self_ref
                        .tell(ConnectionFailed {
                            error: err.to_string(),
                            status: None,
                        })
                        .send()
                        .await;
                }
            };
        });

        Ok(())
    }

    async fn handle_connection_failed(
        &mut self,
        error: String,
        status: Option<u16>,
    ) -> WebSocketResult<()> {
        if let Some(cb) = self.circuit_breaker.as_mut() {
            cb.record_failure();
        }
        self.health.record_internal_error("connect", &error);
        let cause = WsDisconnectCause::HandshakeFailed {
            status,
            message: error.clone(),
        };
        let reason = format!("handshake failed: {error}");
        let action = self.handler.classify_disconnect(&cause);
        self.schedule_reconnect("connection_failed", &reason, &cause, action)
            .await;
        Ok(())
    }

    async fn schedule_reconnect(
        &mut self,
        event: &str,
        reason: &str,
        cause: &WsDisconnectCause,
        action: WsDisconnectAction,
    ) {
        match action {
            WsDisconnectAction::Abort => {
                self.log_reconnect_plan(
                    event,
                    "abort",
                    reason,
                    cause,
                    action,
                    None,
                    self.reconnect_attempt,
                );
                return;
            }
            WsDisconnectAction::ImmediateReconnect | WsDisconnectAction::BackoffReconnect => {}
        }

        if !self.reconnect.should_retry() {
            self.log_reconnect_plan(
                event,
                "retry_suppressed",
                reason,
                cause,
                action,
                None,
                self.reconnect_attempt,
            );
            return;
        }

        let mut delay = match action {
            WsDisconnectAction::ImmediateReconnect => Duration::from_secs(0),
            WsDisconnectAction::BackoffReconnect => jitter_delay(self.reconnect.next_delay()),
            WsDisconnectAction::Abort => Duration::from_secs(0),
        };

        let attempt = self.reconnect_attempt.saturating_add(1);
        self.reconnect_attempt = attempt;
        self.health.increment_reconnect();

        if attempt >= 3 {
            warn!(
                connection = %self.connection_label(),
                attempt,
                "websocket quarantine: too many failed attempts, suspending for 24h"
            );
            delay = Duration::from_secs(86400);
        }

        self.log_reconnect_plan(
            event,
            "scheduled",
            reason,
            cause,
            action,
            Some(delay),
            attempt,
        );

        let actor_ref = self.actor_ref.clone();
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let _ = actor_ref.tell(WebSocketEvent::Connect).send().await;
        });
    }

    async fn on_connection_established(
        &mut self,
        reader: T::Reader,
        writer: T::Writer,
    ) -> WebSocketResult<()> {
        info!("websocket connection established");
        if let Some(cb) = self.circuit_breaker.as_mut() {
            cb.record_success();
        }
        self.health.reset();
        self.inbound_bytes_total = 0;
        self.outbound_bytes_total = 0;
        self.last_inbound_payload_len = None;
        self.last_outbound_payload_len = None;
        self.latency_breach_streak = 0;
        self.reconnect.reset();
        self.ping.reset();
        self.reconnect_attempt = 0;

        let writer_shutdown = self.shutdown_rx.clone();
        let mut reader_shutdown = self.shutdown_rx.clone();
        let mut read = reader;

        if self.writer_supervisor_ref.is_none() {
            self.writer_supervisor_ref = Some(spawn_writer_supervisor::<T::Writer>());
        }
        let writer_sup = self
            .writer_supervisor_ref
            .as_ref()
            .expect("writer supervisor must be set");
        let writer =
            spawn_writer_supervised_with(writer_sup, writer, writer_shutdown).await;
        self.writer_ref = Some(writer);
        self.writer_ready_flag.store(true, Ordering::Release);
        self.writer_ready.notify_waiters();

        self.handler.on_open();

        let reader_actor_ref = self.actor_ref.clone();
        let reader_connection_label = self.connection_label().to_string();
        let mut ingress = self
            .ingress
            .take()
            .expect("ingress must be present when establishing connection");
        ingress.on_open();
        self.reader_task = Some(tokio::spawn(async move {
            const TOUCH_FRAME_BATCH: u64 = 64;
            const TOUCH_INTERVAL: Duration = Duration::from_millis(100);

            let mut frames_since_touch: u64 = 0;
            let mut bytes_since_touch: u64 = 0;
            let mut last_payload_len: Option<usize> = None;
            let mut last_touch = Instant::now();

            loop {
                tokio::select! {
                    res = reader_shutdown.changed() => {
                        if res.is_err() || *reader_shutdown.borrow_and_update() { break; }
                    }
                    message = read.next() => {
                        match message {
                            Some(Ok(WsMessage::Close(frame))) => {
                                info!(
                                    connection = %reader_connection_label,
                                    close = ?frame,
                                    "received websocket close frame"
                                );
                                let reason = frame
                                    .as_ref()
                                    .map(|f| {
                                        format!(
                                            "code={} reason={}",
                                            f.code,
                                            String::from_utf8_lossy(f.reason.as_ref())
                                        )
                                    })
                                    .unwrap_or_else(|| "remote closed".to_string());
                                let _ = reader_actor_ref
                                    .tell(WebSocketEvent::Disconnect {
                                        reason,
                                        cause: WsDisconnectCause::RemoteClosed,
                                    })
                                    .send()
                                    .await;
                                break;
                            }
                            Some(Ok(msg)) => {
                                frames_since_touch = frames_since_touch.saturating_add(1);
                                if let Some(bytes) = message_bytes(&msg) {
                                    bytes_since_touch = bytes_since_touch.saturating_add(bytes.len() as u64);
                                    last_payload_len = Some(bytes.len());
                                }

                                match ingress.on_frame(&msg) {
                                    WsIngressAction::Ignore => {}
                                    WsIngressAction::Emit(message) => {
                                        if reader_actor_ref
                                            .tell(IngressEmit::<E::Message> { message })
                                            .send()
                                            .await
                                            .is_err() {
                                            break;
                                        }
                                    }
                                    WsIngressAction::Forward(frame) => {
                                        if reader_actor_ref.tell(WebSocketEvent::Inbound(frame)).send().await.is_err() {
                                            break;
                                        }
                                    }
                                    WsIngressAction::Reconnect(reason) => {
                                        let _ = reader_actor_ref
                                            .tell(WebSocketEvent::Disconnect {
                                                reason: reason.clone(),
                                                cause: WsDisconnectCause::EndpointRequested { reason },
                                            })
                                            .send()
                                            .await;
                                        break;
                                    }
                                    WsIngressAction::Shutdown(reason) => {
                                        let _ = reader_actor_ref
                                            .tell(WebSocketEvent::Disconnect {
                                                reason: reason.clone(),
                                                cause: WsDisconnectCause::EndpointRequested { reason },
                                            })
                                            .send()
                                            .await;
                                        break;
                                    }
                                }

                                if frames_since_touch >= TOUCH_FRAME_BATCH || last_touch.elapsed() >= TOUCH_INTERVAL {
                                    if frames_since_touch != 0 || bytes_since_touch != 0 {
                                        let _ = reader_actor_ref
                                            .tell(WebSocketEvent::InboundActivity {
                                                frames: frames_since_touch,
                                                bytes: bytes_since_touch,
                                                last_payload_len,
                                            })
                                            .send()
                                            .await;
                                        frames_since_touch = 0;
                                        bytes_since_touch = 0;
                                        last_payload_len = None;
                                    }
                                    last_touch = Instant::now();
                                }
                            }
                            Some(Err(err)) => {
                                let cause = WsDisconnectCause::ReadFailure { error: err.to_string() };
                                let reason = format!("read error: {err}");
                                let _ = reader_actor_ref
                                    .tell(WebSocketEvent::Disconnect { reason, cause })
                                    .send()
                                    .await;
                                break;
                            }
                            None => {
                                let _ = reader_actor_ref
                                    .tell(WebSocketEvent::Disconnect {
                                        reason: "stream ended".to_string(),
                                        cause: WsDisconnectCause::RemoteClosed,
                                    })
                                    .send()
                                    .await;
                                break;
                            }
                        }
                    }
                }
            }

            // Final touch and hand ingress state back to the actor.
            if frames_since_touch != 0 || bytes_since_touch != 0 {
                let _ = reader_actor_ref
                    .tell(WebSocketEvent::InboundActivity {
                        frames: frames_since_touch,
                        bytes: bytes_since_touch,
                        last_payload_len,
                    })
                    .send()
                    .await;
            }

            ingress
        }));

        if self.enable_ping {
            self.start_ping_loop();
        } else {
            info!(
                connection = %self.connection_label(),
                "client ping loop disabled"
            );
        }

        if let Err(err) = self.drain_pending_outbound().await {
            let reason = format!("outbound drain failed: {err}");
            let cause = WsDisconnectCause::InternalError {
                context: "outbound_drain".to_string(),
                error: err.to_string(),
            };
            self.handle_disconnect(reason, cause).await?;
            return Ok(());
        }

        if let Err(err) = self.send_initial_messages().await {
            let reason = format!("initial send failed: {err}");
            let cause = WsDisconnectCause::InternalError {
                context: "initial_send".to_string(),
                error: err.to_string(),
            };
            self.handle_disconnect(reason, cause).await?;
            return Ok(());
        }

        Ok(())
    }

    fn start_ping_loop(&mut self) {
        if let Some(handle) = self.ping_task.take() {
            handle.abort();
        }

        let mut shutdown_rx = self.shutdown_rx.clone();
        let interval = self.ping.interval();
        let actor_ref = self.actor_ref.clone();

        self.ping_task = Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() { break; }
                    }
                    _ = ticker.tick() => {
                        if actor_ref.tell(WebSocketEvent::SendPing).send().await.is_err() {
                            break;
                        }
                        if actor_ref.tell(WebSocketEvent::CheckStale).send().await.is_err() {
                            break;
                        }
                    }
                }
            }
        }));
    }

    async fn send_initial_messages(&mut self) -> WebSocketResult<()> {
        if let Some(auth) = self.handler.generate_auth() {
            if let Err(err) = self.enqueue_message(into_ws_message(auth)).await {
                let reason = format!("auth send failed: {err}");
                let cause = WsDisconnectCause::InternalError {
                    context: "auth_send".to_string(),
                    error: err.to_string(),
                };
                self.handle_disconnect(reason, cause).await?;
                return Ok(());
            }
        }

        let subscriptions = self.handler.subscription_manager().initial_subscriptions();
        for sub in subscriptions {
            let bytes = self
                .handler
                .subscription_manager()
                .serialize_subscription(&sub);
            if let Err(err) = self.enqueue_message(into_ws_message(bytes)).await {
                let reason = format!("subscription send failed: {err}");
                let cause = WsDisconnectCause::InternalError {
                    context: "subscription_send".to_string(),
                    error: err.to_string(),
                };
                self.handle_disconnect(reason, cause).await?;
                break;
            }
        }

        Ok(())
    }

    async fn process_subscription_update(
        &mut self,
        action: WsSubscriptionAction<
            <E::Subscription as WsSubscriptionManager>::SubscriptionMessage,
        >,
    ) -> WebSocketResult<()> {
        let messages = {
            let manager = self.handler.subscription_manager();
            manager.update_subscriptions(action)
        };

        let messages = match messages {
            Ok(msgs) => msgs,
            Err(err) => {
                self.health.record_internal_error("subscriptions", &err);
                return Err(WebSocketError::InvalidState(err));
            }
        };

        for message in messages {
            let payload = {
                let manager = self.handler.subscription_manager();
                manager.serialize_subscription(&message)
            };
            if let Err(err) = self.enqueue_message(into_ws_message(payload)).await {
                let reason = format!("subscription update send failed: {err}");
                let cause = WsDisconnectCause::InternalError {
                    context: "subscription_update".to_string(),
                    error: err.to_string(),
                };
                self.handle_disconnect(reason, cause).await?;
                return Ok(());
            }
        }

        Ok(())
    }

    async fn enqueue_message(&mut self, message: WsMessage) -> WebSocketResult<()> {
        if let Some(limiter) = self.rate_limiter.as_mut() {
            limiter.try_acquire()?;
        }

        if let Some(writer) = self.writer_ref.clone() {
            if let Err(err) = self.drain_pending_outbound().await {
                let reason = format!("outbound drain failed: {err}");
                let cause = WsDisconnectCause::InternalError {
                    context: "outbound_drain".to_string(),
                    error: err.to_string(),
                };
                self.handle_disconnect(reason, cause).await?;
                return Ok(());
            }
            if let Err(err) = self.send_with_writer(writer, message).await {
                let reason = format!("outbound send failed: {err}");
                let cause = WsDisconnectCause::InternalError {
                    context: "outbound_send".to_string(),
                    error: err.to_string(),
                };
                self.handle_disconnect(reason, cause).await?;
                return Ok(());
            }
            return Ok(());
        }

        if self.pending_outbound.len() >= self.outbound_capacity {
            self.health
                .record_internal_error("outbound", "pending queue full");
            return Err(WebSocketError::OutboundQueueFull);
        }

        self.pending_outbound.push_back(message);
        Ok(())
    }

    async fn drain_pending_outbound(&mut self) -> WebSocketResult<()> {
        if self.pending_outbound.is_empty() {
            return Ok(());
        }

        let Some(writer) = self.writer_ref.clone() else {
            return Ok(());
        };

        while let Some(queued) = self.pending_outbound.pop_front() {
            if let Err(err) = self.send_with_writer(writer.clone(), queued).await {
                self.pending_outbound.clear();
                let reason = format!("outbound drain failed: {err}");
                let cause = WsDisconnectCause::InternalError {
                    context: "outbound_drain".to_string(),
                    error: err.to_string(),
                };
                self.handle_disconnect(reason, cause).await?;
                break;
            }
        }
        Ok(())
    }

    async fn send_with_writer(
        &mut self,
        writer: ActorRef<WsWriterActor<T::Writer>>,
        message: WsMessage,
    ) -> WebSocketResult<()> {
        if let Some(bytes) = message_bytes(&message) {
            self.outbound_bytes_total = self
                .outbound_bytes_total
                .saturating_add(bytes.len() as u64);
            self.last_outbound_payload_len = Some(bytes.len());
        }
        match writer.tell(WriterWrite { message }).send().await {
            Ok(_) => {
                self.health.record_sent();
                Ok(())
            }
            Err(err) => {
                let msg = err.to_string();
                self.health.record_internal_error("outbound", &msg);
                warn!(
                    connection = %self.connection_label(),
                    error = %msg,
                    "websocket writer send failed"
                );
                if let Some(metrics) = self.metrics.as_ref() {
                    metrics.track_writer_error(self.connection_label());
                }
                Err(WebSocketError::ActorError(msg))
            }
        }
    }

    async fn handle_disconnect(
        &mut self,
        reason: String,
        cause: WsDisconnectCause,
    ) -> WebSocketResult<()> {
        let action = self.handler.classify_disconnect(&cause);

        self.log_reconnect_plan(
            "disconnect",
            "received",
            &reason,
            &cause,
            action,
            None,
            self.reconnect_attempt,
        );
        if let Some(cb) = self.circuit_breaker.as_mut() {
            cb.record_failure();
        }
        self.stop_io_tasks_only().await;
        self.ping.reset();
        self.latency_breach_streak = 0;
        self.reset_channels();

        self.schedule_reconnect("disconnect", &reason, &cause, action)
            .await;
        Ok(())
    }

    async fn emit_ping(&mut self) -> WebSocketResult<()> {
        if self.status != WsConnectionStatus::Connected
            || !self.writer_ready_flag.load(Ordering::Acquire)
        {
            info!(
                connection = %self.connection_label(),
                status = ?self.status,
                "skipping ping (not connected)"
            );
            return Ok(());
        }

        if self.ping.is_stale() {
            self.handle_disconnect(
                "stale ping detected".to_string(),
                WsDisconnectCause::StaleData,
            )
            .await?;
            return Ok(());
        }

        if let Some(message) = self.ping.create_ping() {
            let payload_len = message_bytes(&message).map(|bytes| bytes.len());
            tracing::debug!(
                connection = %self.connection_label(),
                payload_len = payload_len,
                "sending websocket ping"
            );
            if let Err(err) = self.enqueue_message(message).await {
                let reason = format!("ping send failed: {err}");
                let cause = WsDisconnectCause::InternalError {
                    context: "ping_send".to_string(),
                    error: err.to_string(),
                };
                self.handle_disconnect(reason, cause).await?;
                return Ok(());
            }
        }
        Ok(())
    }

    async fn process_inbound(&mut self, message: WsMessage) -> WebSocketResult<()> {
        self.health.record_message();
        let payload_len = message_bytes(&message).map(|bytes| bytes.len());
        if let Some(bytes_len) = payload_len {
            self.inbound_bytes_total = self
                .inbound_bytes_total
                .saturating_add(bytes_len as u64);
            self.last_inbound_payload_len = Some(bytes_len);
        }
        // Protocol-level ping/pong frames are handled by the ping strategy.
        // Uncomment for per-frame logging:
        /*
        match &message {
            WsMessage::Ping(_) => {
                info!(
                    connection = %self.connection_label(),
                    payload_len = payload_len,
                    payload_hex = payload_hex.as_deref().unwrap_or(""),
                    "received websocket ping"
                );
            }
            WsMessage::Pong(_) => {
                info!(
                    connection = %self.connection_label(),
                    payload_len = payload_len,
                    payload_hex = payload_hex.as_deref().unwrap_or(""),
                    "received websocket pong (frame)"
                );
            }
            _ => {}
        }
        */
        match self.ping.handle_inbound(&message) {
            WsPongResult::PongReceived(Some(rtt)) => {
                // info!(
                //     connection = %self.connection_label(),
                //     payload_len = payload_len,
                //     rtt_us = rtt.as_micros(),
                //     "received websocket pong"
                // );
                self.health.record_rtt(rtt);
                if let Some(breach) = self.evaluate_latency_policy() {
                    self.handle_latency_breach(breach).await?;
                }
            }
            WsPongResult::PongReceived(None) => {
                // info!(
                //     connection = %self.connection_label(),
                //     payload_len = payload_len,
                //     "received websocket pong"
                // );
                if let Some(breach) = self.evaluate_latency_policy() {
                    self.handle_latency_breach(breach).await?;
                }
            }
            WsPongResult::Reply(reply) => {
                // info!(
                //     connection = %self.connection_label(),
                //     payload_len = payload_len,
                //     payload_hex = payload_hex.as_deref().unwrap_or(""),
                //     "replying to websocket ping from server"
                // );
                if let Err(err) = self.enqueue_message(reply).await {
                    let reason = format!("pong reply send failed: {err}");
                    let cause = WsDisconnectCause::InternalError {
                        context: "pong_reply".to_string(),
                        error: err.to_string(),
                    };
                    self.handle_disconnect(reason, cause).await?;
                    return Ok(());
                }
            }
            WsPongResult::InvalidPong => {
                warn!(
                    connection = %self.connection_label(),
                    payload_len = payload_len,
                    "invalid websocket pong received"
                );
                self.health.record_internal_error("ping", "invalid pong");
            }
            WsPongResult::NotPong => {
                if let Some(bytes) = message_bytes(&message) {
                    if bytes.len() > self.buffers.inbound_capacity() {
                        return Err(WebSocketError::ParseFailed(format!(
                            "frame too large: {} > inbound buffer {}",
                            bytes.len(),
                            self.buffers.inbound_capacity()
                        )));
                    }
                    // Zero-copy: parse directly on the tungstenite frame bytes.
                    self.dispatch_endpoint(bytes).await?;
                }
            }
        }

        Ok(())
    }

    async fn dispatch_endpoint(&mut self, bytes: &[u8]) -> WebSocketResult<()> {
        // tracing::info!(target: "solana-ws", len = bytes.len(), "processing dispatch_endpoint");
        match self
            .handler
            .subscription_manager()
            .handle_subscription_response(bytes)
        {
            WsSubscriptionStatus::Acknowledged { success, message } => {
                if !success {
                    let msg = message.unwrap_or_else(|| "subscription failed".to_string());
                    self.health.record_server_error(None, &msg);
                    self.handle_disconnect(
                        msg.clone(),
                        WsDisconnectCause::EndpointRequested { reason: msg },
                    )
                    .await?;
                    return Ok(());
                }
                // For successful acknowledgements, continue and allow handlers to
                // observe the message (some adapters rely on downstream ACK events).
            }
            WsSubscriptionStatus::NotSubscriptionResponse => {}
        }

        match self.handler.parse(bytes) {
            Ok(WsParseOutcome::ServerError {
                code,
                message,
                data,
            }) => {
                self.health.record_server_error(code, &message);
                let action = self.handler.handle_server_error(code, &message, data);
                if let WsErrorAction::Reconnect = action {
                    self.handle_disconnect(
                        message.clone(),
                        WsDisconnectCause::ServerError { code, message },
                    )
                    .await?;
                } else if let WsErrorAction::Fatal = action {
                    return Err(WebSocketError::ServerError { code, message });
                }
            }
            Ok(WsParseOutcome::Message(action)) => {
                self.handle_message_action(action).await?;
            }
            Err(err) => {
                let msg = err.to_string();
                self.health.record_internal_error("parse", &msg);
                return Err(WebSocketError::ParseFailed(msg));
            }
        }

        Ok(())
    }

    async fn handle_message_action(
        &mut self,
        action: WsMessageAction<E::Message>,
    ) -> WebSocketResult<()> {
        match action {
            WsMessageAction::Continue => {}
            WsMessageAction::Process(message) => {
                if let Err(err) = self.handler.handle_message(message) {
                    let msg = err.to_string();
                    self.health.record_internal_error("handle_message", &msg);
                    return Err(WebSocketError::ActorError(msg));
                }
            }
            WsMessageAction::Reconnect(reason) => {
                self.handle_disconnect(
                    reason.clone(),
                    WsDisconnectCause::EndpointRequested { reason },
                )
                .await?;
            }
            WsMessageAction::Shutdown => {
                self.stop_all_tasks().await;
            }
        }
        Ok(())
    }

    async fn handle_server_error(
        &mut self,
        code: Option<i32>,
        message: String,
        data: Option<Value>,
    ) -> WebSocketResult<()> {
        let action = self.handler.handle_server_error(code, &message, data);
        match action {
            WsErrorAction::Continue => Ok(()),
            WsErrorAction::Reconnect => {
                self.handle_disconnect(
                    message.clone(),
                    WsDisconnectCause::ServerError { code, message },
                )
                .await
            }
            WsErrorAction::Fatal => Err(WebSocketError::ServerError { code, message }),
        }
    }

    async fn check_stale(&mut self) -> WebSocketResult<()> {
        if self.ping.is_stale() {
            self.handle_disconnect(
                "stale ping detected".to_string(),
                WsDisconnectCause::StaleData,
            )
            .await?;
            return Ok(());
        }

        if self.health.is_stale() {
            self.handle_disconnect("stale connection".to_string(), WsDisconnectCause::StaleData)
                .await?;
            return Ok(());
        }

        if let Some(breach) = self.evaluate_latency_policy() {
            self.handle_latency_breach(breach).await?;
        }
        Ok(())
    }
    fn evaluate_latency_policy(&mut self) -> Option<LatencyBreach> {
        let stats = self.health.get_stats();
        evaluate_latency_policy_internal(
            self.latency_policy,
            &stats,
            &mut self.latency_breach_streak,
        )
    }

    async fn handle_latency_breach(&mut self, breach: LatencyBreach) -> WebSocketResult<()> {
        let percentile = breach.percentile.as_f64();
        let reason = format!(
            "latency p{percentile:.0} observed {}us (threshold {}us, samples {})",
            breach.observed_us, breach.threshold_us, breach.samples
        );
        let cause = WsDisconnectCause::HighLatency {
            percentile,
            rtt_us: breach.observed_us,
        };
        self.handle_disconnect(reason, cause).await
    }
}

impl<E, R, P, I, T> WebSocketActor<E, R, P, I, T>
where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: WsPingPongStrategy,
    I: WsIngress<Message = E::Message>,
    T: WsTransport,
{
    fn connection_label(&self) -> &str {
        self.registration
            .as_ref()
            .map(|r| r.name.as_str())
            .unwrap_or(&self.url)
    }

    fn log_reconnect_plan(
        &self,
        event: &str,
        note: &str,
        reason: &str,
        cause: &WsDisconnectCause,
        action: WsDisconnectAction,
        delay: Option<Duration>,
        attempt: u64,
    ) {
        let (percentile, rtt_us) = self.latency_snapshot();
        let delay_ms = delay.map(|d| d.as_millis().min(u64::MAX as u128) as u64);
        let stats = self.health.get_stats();
        let last_message_age_ms = stats.last_message_age.as_millis().min(u64::MAX as u128) as u64;
        let uptime_ms = stats.uptime.as_millis().min(u64::MAX as u128) as u64;

        // Fingerprinting (and similar probe phases) intentionally hit many non-WS endpoints.
        // When reconnect is suppressed, this is expected noise and should not be WARN-level.
        if note == "retry_suppressed" {
            tracing::debug!(
                connection = %self.connection_label(),
                url = %self.url,
                event = %event,
                note = %note,
                reason = %reason,
                cause = ?cause,
                action = ?action,
                attempt,
                delay_ms,
                uptime_ms,
                last_message_age_ms,
                messages = stats.messages,
                bytes_in = self.inbound_bytes_total,
                bytes_out = self.outbound_bytes_total,
                last_inbound_len = self.last_inbound_payload_len,
                last_outbound_len = self.last_outbound_payload_len,
                percentile,
                rtt_us,
                "websocket reconnect plan"
            );
        } else {
            warn!(
                connection = %self.connection_label(),
                url = %self.url,
                event = %event,
                note = %note,
                reason = %reason,
                cause = ?cause,
                action = ?action,
                attempt,
                delay_ms,
                uptime_ms,
                last_message_age_ms,
                messages = stats.messages,
                bytes_in = self.inbound_bytes_total,
                bytes_out = self.outbound_bytes_total,
                last_inbound_len = self.last_inbound_payload_len,
                last_outbound_len = self.last_outbound_payload_len,
                percentile,
                rtt_us,
                "websocket reconnect plan"
            );
        }
    }

    fn latency_snapshot(&self) -> (Option<f64>, Option<u64>) {
        let Some(policy) = self.latency_policy else {
            return (None, None);
        };

        let stats = self.health.get_stats();
        let rtt = match policy.percentile {
            WsLatencyPercentile::P50 => stats.p50_latency_us,
            WsLatencyPercentile::P99 => stats.p99_latency_us,
        };

        (Some(policy.percentile.as_f64()), Some(rtt))
    }
}

fn evaluate_latency_policy_internal(
    policy: Option<WsLatencyPolicy>,
    stats: &WsConnectionStats,
    streak: &mut u32,
) -> Option<LatencyBreach> {
    let Some(policy) = policy else {
        return None;
    };

    let observed = match policy.percentile {
        WsLatencyPercentile::P50 => stats.p50_latency_us,
        WsLatencyPercentile::P99 => stats.p99_latency_us,
    };

    let threshold_us = policy.threshold.as_micros().min(u64::MAX as u128) as u64;

    if stats.latency_samples < policy.min_samples {
        info!(
            percentile = policy.percentile.as_f64(),
            observed_us = observed,
            threshold_us,
            samples = stats.latency_samples,
            min_samples = policy.min_samples,
            "latency policy awaiting minimum samples"
        );
        *streak = 0;
        return None;
    }

    if observed > threshold_us {
        *streak = streak.saturating_add(1);
        info!(
            percentile = policy.percentile.as_f64(),
            observed_us = observed,
            threshold_us,
            samples = stats.latency_samples,
            breach_streak = *streak,
            required_streak = policy.consecutive_breaches,
            "latency policy breach observed"
        );
        if *streak >= policy.consecutive_breaches {
            info!(
                percentile = policy.percentile.as_f64(),
                observed_us = observed,
                threshold_us,
                samples = stats.latency_samples,
                "latency policy disconnect triggered"
            );
            Some(LatencyBreach {
                percentile: policy.percentile,
                observed_us: observed,
                threshold_us,
                samples: stats.latency_samples,
            })
        } else {
            None
        }
    } else {
        info!(
            percentile = policy.percentile.as_f64(),
            observed_us = observed,
            threshold_us,
            samples = stats.latency_samples,
            "latency within threshold"
        );
        *streak = 0;
        None
    }
}

#[cfg(test)]
mod latency_tests {
    use super::*;
    use std::{thread, time::Duration};

    fn make_stats(p50: u64, p99: u64, samples: u64) -> WsConnectionStats {
        WsConnectionStats {
            uptime: Duration::from_secs(0),
            messages: 0,
            errors: 0,
            reconnects: 0,
            last_message_age: Duration::from_secs(0),
            recent_server_errors: 0,
            recent_internal_errors: 0,
            p50_latency_us: p50,
            p99_latency_us: p99,
            latency_samples: samples,
        }
    }

    #[test]
    fn rate_limiter_blocks_and_recovers() {
        let mut limiter = WsRateLimiter::new(2, Duration::from_millis(20));

        assert!(limiter.try_acquire().is_ok());
        assert!(limiter.try_acquire().is_ok());

        match limiter.try_acquire() {
            Err(WebSocketError::RateLimited {
                retry_after: Some(retry_after),
                ..
            }) => {
                assert!(retry_after <= Duration::from_millis(20));
            }
            other => panic!("unexpected result: {other:?}"),
        }

        thread::sleep(Duration::from_millis(25));
        assert!(limiter.try_acquire().is_ok());
    }

    #[test]
    fn circuit_breaker_transitions_through_states() {
        let mut breaker = WsCircuitBreaker::new(2, 1, Duration::from_millis(30));

        assert!(breaker.can_proceed());
        breaker.record_failure();
        breaker.record_failure();

        assert!(!breaker.can_proceed());
        let wait = breaker.time_until_retry().expect("breaker should be open");
        assert!(wait <= Duration::from_millis(30));

        thread::sleep(Duration::from_millis(35));
        assert!(breaker.can_proceed());

        breaker.record_success();
        assert!(breaker.can_proceed());

        breaker.record_failure();
        breaker.record_failure();
        assert!(!breaker.can_proceed());

        thread::sleep(Duration::from_millis(35));
        assert!(breaker.can_proceed());
        breaker.record_failure();
        assert!(!breaker.can_proceed());
    }

    #[test]
    fn latency_policy_requires_consecutive_breaches() {
        let policy = WsLatencyPolicy {
            percentile: WsLatencyPercentile::P50,
            threshold: Duration::from_millis(50),
            min_samples: 2,
            consecutive_breaches: 2,
        };
        let mut streak = 0;

        let insufficient = make_stats(60_000, 60_000, 1);
        assert!(
            evaluate_latency_policy_internal(Some(policy), &insufficient, &mut streak).is_none()
        );
        assert_eq!(streak, 0);

        let elevated = make_stats(60_000, 60_000, 3);
        assert!(evaluate_latency_policy_internal(Some(policy), &elevated, &mut streak).is_none());
        assert_eq!(streak, 1);

        let breach = evaluate_latency_policy_internal(Some(policy), &elevated, &mut streak)
            .expect("second breach should trigger disconnect");
        assert_eq!(breach.percentile, WsLatencyPercentile::P50);
        assert_eq!(breach.observed_us, 60_000);
        assert_eq!(breach.threshold_us, 50_000);
        assert_eq!(breach.samples, 3);
    }

    #[test]
    fn latency_policy_resets_after_recovery() {
        let policy = WsLatencyPolicy {
            percentile: WsLatencyPercentile::P99,
            threshold: Duration::from_millis(40),
            min_samples: 1,
            consecutive_breaches: 3,
        };

        let mut streak = 0;
        let breach_stats = make_stats(20_000, 50_000, 5);
        assert!(
            evaluate_latency_policy_internal(Some(policy), &breach_stats, &mut streak).is_none()
        );
        assert_eq!(streak, 1);

        let recovered_stats = make_stats(20_000, 35_000, 5);
        assert!(
            evaluate_latency_policy_internal(Some(policy), &recovered_stats, &mut streak).is_none()
        );
        assert_eq!(streak, 0);
    }
}

/// Events processed by the websocket actor.
#[derive(Debug)]
pub enum WebSocketEvent {
    Connect,
    Disconnect {
        reason: String,
        cause: WsDisconnectCause,
    },
    Inbound(WsMessage),
    /// Lightweight inbound activity update emitted by the IO loop even when frames are ignored.
    InboundActivity {
        frames: u64,
        bytes: u64,
        last_payload_len: Option<usize>,
    },
    SendMessage(WsMessage),
    SendBatch(Vec<WsMessage>),
    ServerError {
        code: Option<i32>,
        message: String,
        data: Option<Value>,
    },
    InternalError {
        context: String,
        error: String,
    },
    SendPing,
    CheckStale,
    ResetState,
    UpdateConnectionStatus(WsConnectionStatus),
}

/// Message to allow external callers to wait until the writer is ready.
#[derive(Clone, Copy, Debug)]
pub struct WaitForWriter {
    pub timeout: Duration,
}

/// Message for mutating the active subscription set managed by the handler.
#[derive(Debug)]
pub struct WsSubscriptionUpdate<M>
where
    M: Send + 'static,
{
    pub action: WsSubscriptionAction<M>,
}

impl<E, R, P, I, T>
    KameoMessage<
        WsSubscriptionUpdate<<E::Subscription as WsSubscriptionManager>::SubscriptionMessage>,
    > for WebSocketActor<E, R, P, I, T>
where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: WsPingPongStrategy,
    I: WsIngress<Message = E::Message>,
    T: WsTransport,
{
    type Reply = WebSocketResult<()>;

    async fn handle(
        &mut self,
        update: WsSubscriptionUpdate<
            <E::Subscription as WsSubscriptionManager>::SubscriptionMessage,
        >,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.process_subscription_update(update.action).await
    }
}

impl<E, R, P, I, T> KameoMessage<WaitForWriter> for WebSocketActor<E, R, P, I, T>
where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: WsPingPongStrategy,
    I: WsIngress<Message = E::Message>,
    T: WsTransport,
{
    type Reply = WebSocketResult<()>;

    async fn handle(
        &mut self,
        msg: WaitForWriter,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if self.writer_ready_flag.load(Ordering::Acquire) || self.writer_ref.is_some() {
            return Ok(());
        }
        match tokio::time::timeout(msg.timeout, self.writer_ready.notified()).await {
            Ok(_) => Ok(()),
            Err(_) => Err(WebSocketError::Timeout {
                context: format!(
                    "writer not ready for {} within {:?}",
                    self.connection_label(),
                    msg.timeout
                ),
            }),
        }
    }
}
