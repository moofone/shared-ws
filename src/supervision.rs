//! Minimal typed supervisor.
//!
//! This is intentionally lightweight: websocket actors are typically
//! "restart-by-reconnect" (the connection is recreated, not the actor), so we
//! don't implement automatic restart policies here. The primary purpose is to
//! provide a stable, actor-managed parent for linking child actors.

use std::convert::Infallible;
use std::marker::PhantomData;
use std::ops::ControlFlow;

use kameo::{
    Actor,
    actor::{ActorId, ActorRef, WeakActorRef},
    error::ActorStopReason,
};

/// Typed link-based supervisor for homogeneous actors.
pub struct TypedSupervisor<A>
where
    A: Actor + Send + Sync + 'static,
{
    _name: String,
    _spawn_fn: Box<dyn Fn(&str) -> ActorRef<A> + Send + Sync>,
    _phantom: PhantomData<A>,
}

impl<A> TypedSupervisor<A>
where
    A: Actor + Send + Sync + 'static,
{
    pub fn new(
        name: impl Into<String>,
        _alerts: Option<()>,
        spawn_fn: impl Fn(&str) -> ActorRef<A> + Send + Sync + 'static,
    ) -> Self {
        Self {
            _name: name.into(),
            _spawn_fn: Box::new(spawn_fn),
            _phantom: PhantomData,
        }
    }
}

impl<A> Actor for TypedSupervisor<A>
where
    A: Actor + Send + Sync + 'static,
{
    type Args = Self;
    type Error = Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(args)
    }

    fn on_link_died(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _id: ActorId,
        _reason: ActorStopReason,
    ) -> impl std::future::Future<
        Output = Result<ControlFlow<ActorStopReason>, Self::Error>,
    > + Send {
        async { Ok(ControlFlow::Continue(())) }
    }
}

