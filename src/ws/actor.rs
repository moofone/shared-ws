//! High-performance websocket actor.
//!
//! The websocket IO loop runs outside kameo for maximum throughput; the actor owns
//! connection state/policies and receives frames via messages.

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use sonic_rs::Value;
use tokio::{
    sync::{Notify, watch},
    task::JoinHandle,
    time::MissedTickBehavior,
};
use tracing::{info, warn};

use super::WsCircuitBreaker;
use super::WsMetricsHook;
use super::delegated::{WsDelegatedError, WsDelegatedOk, WsDelegatedRequest};
use super::delegated_pending::{PendingInsertOutcome, PendingTable};
use super::health::WsHealthMonitor;
use super::ping::{WsPingPongStrategy, WsPongResult};
use super::types::{
    ForwardAllIngress, WebSocketBufferConfig, WebSocketError, WebSocketResult, WsActorRegistration,
    WsBufferPool, WsConnectionStats, WsConnectionStatus, WsDisconnectAction, WsDisconnectCause,
    WsEndpointHandler, WsErrorAction, WsIngress, WsIngressAction, WsLatencyPercentile,
    WsLatencyPolicy, WsMessage, WsMessageAction, WsParseOutcome, WsPayloadLatencySamplingConfig,
    WsReconnectStrategy, WsRequestMatch, WsSubscriptionAction, WsSubscriptionManager,
    WsSubscriptionStatus, WsTlsConfig, into_ws_message, message_bytes,
};
use super::writer::{
    WriterWrite, WsWriterActor, spawn_writer_supervised_with, spawn_writer_supervisor,
};
use crate::core::connection_policy::jitter_delay;
use crate::supervision::TypedSupervisor;
use crate::transport::WsTransport;
use crate::transport::tungstenite::TungsteniteTransport;
use kameo::error::RegistryError;
use kameo::prelude::{Actor, ActorRef, Context, Message as KameoMessage, WeakActorRef};
use kameo::reply::{DelegatedReply, ReplySender};

const DEFAULT_MAX_PENDING_REQUESTS: usize = 1024;

#[derive(Clone, Copy, Debug)]
struct LatencyBreach {
    percentile: WsLatencyPercentile,
    observed_us: u64,
    threshold_us: u64,
    samples: u64,
}

/// Arguments passed when constructing a websocket actor instance.
pub struct WebSocketActorArgs<
    E,
    R,
    P,
    I = ForwardAllIngress<<E as WsEndpointHandler>::Message>,
    T = TungsteniteTransport,
> where
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
    pub circuit_breaker: Option<WsCircuitBreaker>,
    pub latency_policy: Option<WsLatencyPolicy>,
    /// Opt-in distributed instrumentation: sample payload timestamp lag at low frequency.
    ///
    /// Effective only if `metrics` is also set.
    pub payload_latency_sampling: Option<WsPayloadLatencySamplingConfig>,
    pub registration: Option<WsActorRegistration>,
    pub metrics: Option<WsMetricsHook>,
}

/// Skeleton websocket actor that will be fleshed out in later sprints.
pub struct WebSocketActor<
    E,
    R,
    P,
    I = ForwardAllIngress<<E as WsEndpointHandler>::Message>,
    T = TungsteniteTransport,
> where
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
    writer_ready: Notify,
    pub outbound_capacity: usize,
    pub circuit_breaker: Option<WsCircuitBreaker>,
    pub latency_policy: Option<WsLatencyPolicy>,
    pub latency_breach_streak: u32,
    pub status: WsConnectionStatus,
    payload_latency_sampling: Option<WsPayloadLatencySamplingConfig>,
    next_payload_latency_sample_at: Instant,
    pub registration: Option<WsActorRegistration>,
    pub reconnect_attempt: u64,
    pub metrics: Option<WsMetricsHook>,
    pending_outbound: VecDeque<WsMessage>,
    delegated_pending: PendingTable<ReplySender<Result<WsDelegatedOk, WsDelegatedError>>>,
    delegated_expiry_scheduled_at: Option<Instant>,
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
            circuit_breaker,
            latency_policy,
            payload_latency_sampling,
            registration,
            metrics,
        } = args;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let buffers = WsBufferPool::new(ws_buffers);

        if let Some(registration) = registration.as_ref() {
            let actor_name = &registration.name;
            // We only need local discovery for WS pools; distributed registration requires
            // implementing `ActorRegistration`, which we intentionally avoid here.
            match ctx.register_local(actor_name.clone()) {
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

        // Avoid an extra refcount bump: we can move `ctx` into the actor after registration.
        let actor_ref = ctx;
        let pending_outbound = VecDeque::with_capacity(outbound_capacity);
        let delegated_pending =
            PendingTable::<ReplySender<Result<WsDelegatedOk, WsDelegatedError>>>::new(
                DEFAULT_MAX_PENDING_REQUESTS,
            );
        let health = WsHealthMonitor::new(stale_threshold);

        Ok(Self {
            url,
            tls,
            transport,
            health,
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
            writer_ready: Notify::new(),
            outbound_capacity,
            circuit_breaker,
            latency_policy,
            latency_breach_streak: 0,
            status: WsConnectionStatus::Connecting,
            payload_latency_sampling,
            next_payload_latency_sample_at: Instant::now(),
            registration,
            reconnect_attempt: 0,
            metrics,
            pending_outbound,
            delegated_pending,
            delegated_expiry_scheduled_at: None,
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

    #[allow(clippy::manual_async_fn)]
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
            WebSocketEvent::DelegatedWriteDone {
                request_id,
                ok,
                error,
            } => {
                if ok {
                    let waiters = self
                        .delegated_pending
                        .mark_sent_ok(request_id)
                        .unwrap_or_default();
                    for waiter in waiters {
                        waiter.send(Ok(WsDelegatedOk {
                            request_id,
                            confirmed: false,
                            rate_limit_feedback: None,
                        }));
                    }
                } else {
                    let waiters = self
                        .delegated_pending
                        .complete_not_delivered(request_id)
                        .unwrap_or_default();
                    let msg = error.unwrap_or_else(|| "writer send failed".to_string());
                    for waiter in waiters {
                        waiter.send(Err(WsDelegatedError::not_delivered(
                            request_id,
                            msg.clone(),
                        )));
                    }
                }
                self.schedule_delegated_expiry();
            }
            WebSocketEvent::DelegatedExpire => {
                self.delegated_expiry_scheduled_at = None;
                let now = Instant::now();
                let expired = self.delegated_pending.expire_due(now);
                for entry in expired {
                    let msg = if entry.sent_ok {
                        "deadline elapsed awaiting confirmation".to_string()
                    } else {
                        "deadline elapsed awaiting send".to_string()
                    };
                    for waiter in entry.waiters {
                        waiter.send(Err(WsDelegatedError::unconfirmed(
                            entry.request_id,
                            msg.clone(),
                        )));
                    }
                }
                self.schedule_delegated_expiry();
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
            WebSocketEvent::DrainOutbound => {
                if let Err(err) = self.drain_pending_outbound().await {
                    let reason = format!("outbound drain failed: {err}");
                    let cause = WsDisconnectCause::InternalError {
                        context: "outbound_drain".to_string(),
                        error: err.to_string(),
                    };
                    self.handle_disconnect(reason, cause).await?;
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

#[derive(Debug)]
pub struct IngressEmitBatch<M: Send + 'static> {
    pub messages: Vec<M>,
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

impl<E, R, P, I, T> KameoMessage<IngressEmitBatch<E::Message>> for WebSocketActor<E, R, P, I, T>
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
        msg: IngressEmitBatch<E::Message>,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        for m in msg.messages {
            self.handle_message_action(WsMessageAction::Process(m))
                .await?;
        }
        Ok(())
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

pub struct GetConnectionStatus;

impl<E, R, P, I, T> KameoMessage<GetConnectionStatus> for WebSocketActor<E, R, P, I, T>
where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: WsPingPongStrategy,
    I: WsIngress<Message = E::Message>,
    T: WsTransport,
{
    type Reply = WebSocketResult<WsConnectionStatus>;

    async fn handle(
        &mut self,
        _message: GetConnectionStatus,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.status)
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
        if let Some(handle) = handle.take()
            && let Err(err) = handle.await
        {
            warn!("task terminated with error: {err}");
        }
    }

    async fn await_reader(handle: &mut Option<JoinHandle<I>>) -> Option<I> {
        let handle = handle.take()?;
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
        self.writer_ready = Notify::new();
    }

    fn reset_shutdown_channel(&mut self) {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = shutdown_tx;
        self.shutdown_rx = shutdown_rx;
    }

    async fn teardown_writer(&mut self) {
        let writer = self.writer_ref.take();

        if let (Some(writer), Some(supervisor)) = (&writer, self.writer_supervisor_ref.as_ref()) {
            let _ = writer.stop_gracefully().await;
            writer.wait_for_shutdown().await;
            writer.unlink(supervisor).await;
        }
    }

    async fn handle_connect(&mut self) -> WebSocketResult<()> {
        self.health.record_connect_attempt();
        if let Some(cb) = self.circuit_breaker.as_mut()
            && !cb.can_proceed()
        {
            if let Some(delay) = cb.time_until_retry() {
                let actor_ref = self.actor_ref.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let _ = actor_ref.tell(WebSocketEvent::Connect).send().await;
                });
            }
            return Ok(());
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

        let delay = match action {
            WsDisconnectAction::ImmediateReconnect => Duration::from_secs(0),
            WsDisconnectAction::BackoffReconnect => jitter_delay(self.reconnect.next_delay()),
            WsDisconnectAction::Abort => Duration::from_secs(0),
        };

        let attempt = self.reconnect_attempt.saturating_add(1);
        self.reconnect_attempt = attempt;
        self.health.increment_reconnect();

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
        self.health.record_connect_success();
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
        let writer = spawn_writer_supervised_with(writer_sup, writer, writer_shutdown).await;
        self.writer_ref = Some(writer);
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

                                match ingress.on_frame(msg) {
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
                                    WsIngressAction::EmitBatch(messages) => {
                                        if reader_actor_ref
                                            .tell(IngressEmitBatch::<E::Message> { messages })
                                            .send()
                                            .await
                                            .is_err()
                                        {
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
        if let Some(auth) = self.handler.generate_auth()
            && let Err(err) = self.enqueue_message(into_ws_message(auth)).await
        {
            let reason = format!("auth send failed: {err}");
            let cause = WsDisconnectCause::InternalError {
                context: "auth_send".to_string(),
                error: err.to_string(),
            };
            self.handle_disconnect(reason, cause).await?;
            return Ok(());
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
        if self.pending_outbound.len() >= self.outbound_capacity {
            self.health
                .record_internal_error("outbound", "pending queue full");
            return Err(WebSocketError::OutboundQueueFull);
        }

        self.pending_outbound.push_back(message);
        self.drain_pending_outbound().await
    }

    async fn drain_pending_outbound(&mut self) -> WebSocketResult<()> {
        if self.pending_outbound.is_empty() {
            return Ok(());
        }

        let Some(writer) = self.writer_ref.clone() else {
            return Ok(());
        };

        while !self.pending_outbound.is_empty() {
            let queued = self
                .pending_outbound
                .pop_front()
                .expect("front just existed");
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

    fn schedule_delegated_expiry(&mut self) {
        let Some(next_deadline) = self.delegated_pending.next_deadline() else {
            self.delegated_expiry_scheduled_at = None;
            return;
        };

        let now = Instant::now();
        let delay = next_deadline
            .checked_duration_since(now)
            .unwrap_or(Duration::ZERO);
        let when = now + delay;

        if let Some(existing) = self.delegated_expiry_scheduled_at
            && existing <= when
        {
            return;
        }

        self.delegated_expiry_scheduled_at = Some(when);
        let actor_ref = self.actor_ref.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = actor_ref.tell(WebSocketEvent::DelegatedExpire).send().await;
        });
    }

    async fn send_with_writer(
        &mut self,
        writer: ActorRef<WsWriterActor<T::Writer>>,
        message: WsMessage,
    ) -> WebSocketResult<()> {
        if let Some(bytes) = message_bytes(&message) {
            self.outbound_bytes_total =
                self.outbound_bytes_total.saturating_add(bytes.len() as u64);
            self.last_outbound_payload_len = Some(bytes.len());
        }
        match writer.ask(WriterWrite { message }).await {
            Ok(()) => {
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
        if self.status != WsConnectionStatus::Connected || self.writer_ref.is_none() {
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
            self.inbound_bytes_total = self.inbound_bytes_total.saturating_add(bytes_len as u64);
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
                    // This is a transport-independent guardrail. Do not tie it to the actor's
                    // scratch buffer size: in the zero-copy path we parse directly on transport
                    // bytes, and transports may allow `max_message_bytes > read_buffer_bytes`.
                    if bytes.len() > self.ws_buffers.max_message_bytes {
                        return Err(WebSocketError::ParseFailed(format!(
                            "frame too large: {} > max message {}",
                            bytes.len(),
                            self.ws_buffers.max_message_bytes
                        )));
                    }
                    // Zero-copy: parse directly on the tungstenite frame bytes.
                    self.dispatch_endpoint(&message).await?;
                }
            }
        }

        Ok(())
    }

    async fn dispatch_endpoint(&mut self, frame: &WsMessage) -> WebSocketResult<()> {
        let Some(bytes) = message_bytes(frame) else {
            return Ok(());
        };
        // tracing::info!(target: "solana-ws", len = bytes.len(), "processing dispatch_endpoint");
        if self
            .handler
            .subscription_manager()
            .maybe_subscription_response(bytes)
        {
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
        }

        if !self.delegated_pending.is_empty()
            && self.handler.maybe_request_response(bytes)
            && let Some(req_match) = self.handler.match_request_response(bytes)
        {
            self.complete_delegated_from_match(req_match);
        }

        match self.handler.parse_frame(frame) {
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
                // Opt-in distributed instrumentation: sample payload timestamp lag at low
                // frequency, and only if a metrics hook is configured.
                if let (Some(cfg), Some(metrics)) =
                    (self.payload_latency_sampling, self.metrics.as_ref())
                    && cfg.interval > Duration::ZERO
                {
                    let now = Instant::now();
                    if now >= self.next_payload_latency_sample_at {
                        self.next_payload_latency_sample_at = now + cfg.interval;
                        if let Some(now_epoch_us) = WsPayloadLatencySamplingConfig::now_epoch_us() {
                            // Split field borrows: we want an immutable connection id while
                            // mutably borrowing the handler.
                            let connection = self
                                .registration
                                .as_ref()
                                .map(|r| r.name.as_str())
                                .unwrap_or(self.url.as_str());

                            self.handler.sample_payload_timestamps_us(
                                bytes,
                                &mut |kind, payload_ts_us| {
                                    let lag_us = now_epoch_us.saturating_sub(payload_ts_us);
                                    if lag_us >= 0 {
                                        metrics.observe_payload_lag_us(
                                            connection,
                                            kind,
                                            lag_us as u64,
                                        );
                                    }
                                },
                            );
                        }
                    }
                }
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

    fn complete_delegated_from_match(&mut self, req_match: WsRequestMatch) {
        let request_id = req_match.request_id;
        match req_match.result {
            Ok(()) => {
                let waiters = self
                    .delegated_pending
                    .complete_confirmed(request_id)
                    .unwrap_or_default();
                for waiter in waiters {
                    waiter.send(Ok(WsDelegatedOk {
                        request_id,
                        confirmed: true,
                        rate_limit_feedback: req_match.rate_limit_feedback.clone(),
                    }));
                }
            }
            Err(message) => {
                let waiters = self
                    .delegated_pending
                    .complete_rejected(request_id)
                    .unwrap_or_default();
                for waiter in waiters {
                    waiter.send(Err(WsDelegatedError::endpoint_rejected(
                        request_id,
                        message.clone(),
                        req_match.rate_limit_feedback.clone(),
                    )));
                }
            }
        }
        self.schedule_delegated_expiry();
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

    #[allow(clippy::too_many_arguments)]
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
    let policy = policy?;

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
            connect_attempts: 0,
            connect_successes: 0,
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
    /// Attempt to drain any queued outbound messages (used for rate-limit retries).
    DrainOutbound,
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
    /// Completion event for delegated requests after observing a writer send result.
    DelegatedWriteDone {
        request_id: u64,
        ok: bool,
        error: Option<String>,
    },
    /// Timer tick to expire delegated pending requests by deadline.
    DelegatedExpire,
    ResetState,
    UpdateConnectionStatus(WsConnectionStatus),
}

/// Message to allow external callers to wait until the writer is ready.
#[derive(Clone, Copy, Debug)]
pub struct WaitForWriter {
    pub timeout: Duration,
}

/// Public ask-able request API: send an outbound frame and await a terminal outcome.
///
/// Sprint 1 supports:
/// - deterministic `NotDelivered` (writer send error)
/// - `Unconfirmed` timeouts for `confirm_mode=Confirmed` (no matcher yet)
impl<E, R, P, I, T> KameoMessage<WsDelegatedRequest> for WebSocketActor<E, R, P, I, T>
where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: WsPingPongStrategy,
    I: WsIngress<Message = E::Message>,
    T: WsTransport,
{
    type Reply = DelegatedReply<Result<WsDelegatedOk, WsDelegatedError>>;

    async fn handle(
        &mut self,
        req: WsDelegatedRequest,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let Some(writer) = self.writer_ref.as_ref().cloned() else {
            return ctx.reply(Err(WsDelegatedError::not_delivered(
                req.request_id,
                "writer not ready",
            )));
        };

        let (delegated_reply, waiter) = ctx.reply_sender();
        let (outcome, waiter) = self.delegated_pending.insert_or_join(
            req.request_id,
            req.fingerprint,
            req.confirm_deadline,
            req.confirm_mode,
            waiter,
        );

        match outcome {
            PendingInsertOutcome::Inserted => {
                self.schedule_delegated_expiry();
                let actor_ref = self.actor_ref.clone();
                let request_id = req.request_id;
                let message = req.frame;
                tokio::spawn(async move {
                    let result = writer.ask(WriterWrite { message }).await;
                    match result {
                        Ok(()) => {
                            let _ = actor_ref
                                .tell(WebSocketEvent::DelegatedWriteDone {
                                    request_id,
                                    ok: true,
                                    error: None,
                                })
                                .send()
                                .await;
                        }
                        Err(err) => {
                            let _ = actor_ref
                                .tell(WebSocketEvent::DelegatedWriteDone {
                                    request_id,
                                    ok: false,
                                    error: Some(err.to_string()),
                                })
                                .send()
                                .await;
                        }
                    }
                });
            }
            PendingInsertOutcome::Joined => {
                // Joining may have tightened the deadline; ensure expiry is scheduled.
                self.schedule_delegated_expiry();
            }
            PendingInsertOutcome::JoinedAlreadySent => {
                if let Some(waiter) = waiter {
                    waiter.send(Ok(WsDelegatedOk {
                        request_id: req.request_id,
                        confirmed: false,
                        rate_limit_feedback: None,
                    }));
                }
                // Joining may have tightened the deadline for confirmed waiters (if any).
                self.schedule_delegated_expiry();
            }
            PendingInsertOutcome::PayloadMismatch => {
                if let Some(waiter) = waiter {
                    waiter.send(Err(WsDelegatedError::PayloadMismatch {
                        request_id: req.request_id,
                    }));
                }
            }
            PendingInsertOutcome::TooManyPending => {
                if let Some(waiter) = waiter {
                    waiter.send(Err(WsDelegatedError::TooManyPending {
                        max: DEFAULT_MAX_PENDING_REQUESTS,
                    }));
                }
            }
        }

        delegated_reply
    }
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
        if self.writer_ref.is_some() {
            return Ok(());
        }

        let deadline = Instant::now() + msg.timeout;
        loop {
            if self.writer_ref.is_some() {
                return Ok(());
            }

            // Subscribe first, then re-check to avoid missing a notify between check and await.
            let notified = self.writer_ready.notified();
            if self.writer_ref.is_some() {
                return Ok(());
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(WebSocketError::Timeout {
                    context: format!(
                        "writer not ready for {} within {:?}",
                        self.connection_label(),
                        msg.timeout
                    ),
                });
            }

            let remaining = deadline.duration_since(now);
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return Err(WebSocketError::Timeout {
                    context: format!(
                        "writer not ready for {} within {:?}",
                        self.connection_label(),
                        msg.timeout
                    ),
                });
            }
        }
    }
}
