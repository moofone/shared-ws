use super::{WebSocketError, WebSocketResult, WsMessage};
use crate::supervision::TypedSupervisor;
use futures_util::{Sink, SinkExt};
use kameo::prelude::{Actor, ActorRef, Context, Message as KameoMessage};
use tokio::sync::watch;
use tracing::debug;

/// Writer actor that owns the transport writer and serializes writes.
pub struct WsWriterActor<W>
where
    W: Sink<WsMessage, Error = WebSocketError> + Send + Sync + Unpin + 'static,
{
    writer: W,
    shutdown_rx: watch::Receiver<bool>,
}

impl<W> WsWriterActor<W>
where
    W: Sink<WsMessage, Error = WebSocketError> + Send + Sync + Unpin + 'static,
{
    pub fn new(writer: W, shutdown_rx: watch::Receiver<bool>) -> Self {
        Self {
            writer,
            shutdown_rx,
        }
    }
}

impl<W> Actor for WsWriterActor<W>
where
    W: Sink<WsMessage, Error = WebSocketError> + Send + Sync + Unpin + 'static,
{
    type Args = Self;
    type Error = WebSocketError;

    async fn on_start(args: Self::Args, _ctx: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(args)
    }

    fn on_panic(
        &mut self,
        _actor_ref: kameo::actor::WeakActorRef<Self>,
        err: kameo::prelude::PanicError,
    ) -> impl std::future::Future<
        Output = Result<std::ops::ControlFlow<kameo::prelude::ActorStopReason>, Self::Error>,
    > + Send {
        async move {
            tracing::error!(error = ?err, "WsWriterActor panicked");
            Ok(std::ops::ControlFlow::Break(
                kameo::prelude::ActorStopReason::Panicked(err),
            ))
        }
    }
}

#[derive(Clone)]
pub struct WriterWrite {
    pub message: WsMessage,
}

impl<W> KameoMessage<WriterWrite> for WsWriterActor<W>
where
    W: Sink<WsMessage, Error = WebSocketError> + Send + Sync + Unpin + 'static,
{
    type Reply = WebSocketResult<()>;

    async fn handle(
        &mut self,
        msg: WriterWrite,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if *self.shutdown_rx.borrow() {
            return Err(WebSocketError::InvalidState("writer stopped".to_string()));
        }
        debug!(target: "ws-writer", "sending websocket frame to wire");
        self.writer.send(msg.message).await?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct WriterWriteBatch {
    pub messages: Vec<WsMessage>,
}

impl<W> KameoMessage<WriterWriteBatch> for WsWriterActor<W>
where
    W: Sink<WsMessage, Error = WebSocketError> + Send + Sync + Unpin + 'static,
{
    type Reply = WebSocketResult<()>;

    async fn handle(
        &mut self,
        msg: WriterWriteBatch,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if *self.shutdown_rx.borrow() {
            return Err(WebSocketError::InvalidState("writer stopped".to_string()));
        }
        for frame in msg.messages {
            self.writer.feed(frame).await?;
        }
        self.writer.flush().await?;
        Ok(())
    }
}

/// Spawn a per-crate supervisor for writer instances.
pub fn spawn_writer_supervisor<W>() -> ActorRef<TypedSupervisor<WsWriterActor<W>>>
where
    W: Sink<WsMessage, Error = WebSocketError> + Send + Sync + Unpin + 'static,
{
    // For WS writers, restart requires a new connection; supervisor will not auto-restart.
    TypedSupervisor::spawn(TypedSupervisor::new(
        "shared-ws-writer",
        None,
        |_name| -> ActorRef<WsWriterActor<W>> {
            unreachable!("WsWriterActor restart unsupported; new connection must recreate actor")
        },
    ))
}

/// Spawn a writer and link it to an existing supervisor.
pub async fn spawn_writer_supervised_with<W>(
    supervisor: &ActorRef<TypedSupervisor<WsWriterActor<W>>>,
    writer: W,
    shutdown_rx: watch::Receiver<bool>,
) -> ActorRef<WsWriterActor<W>>
where
    W: Sink<WsMessage, Error = WebSocketError> + Send + Sync + Unpin + 'static,
{
    let actor = WsWriterActor::spawn(WsWriterActor::new(writer, shutdown_rx));
    actor.link(supervisor).await;
    actor
}
