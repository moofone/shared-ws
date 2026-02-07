use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use kameo::prelude::{Actor, ActorRef, Context, Message as KameoMessage};

use crate::core::rate_limit::WsRateLimiter;
use crate::core::types::{WebSocketError, WebSocketResult};
use crate::supervision::TypedSupervisor;

/// Stable provider identifier used for global rate limiting.
///
/// Callers should construct this once (e.g. `Arc::<str>::from("bybit")`) and reuse it
/// across websocket clients to avoid repeated allocations.
pub type WsProviderId = Arc<str>;

#[derive(Clone, Copy, Debug)]
pub struct WsRateLimitConfig {
    pub max_per_window: u32,
    pub window: Duration,
}

impl WsRateLimitConfig {
    pub fn new(max_per_window: u32, window: Duration) -> Self {
        Self {
            max_per_window,
            window,
        }
    }
}

/// Handle carried by websocket actors to participate in a global per-provider limiter.
#[derive(Clone, Debug)]
pub struct WsGlobalRateLimitHandle {
    pub provider: WsProviderId,
    pub limiter: ActorRef<WsGlobalRateLimiterActor>,
    /// How many permits to request at once to amortize actor messaging.
    pub batch: u32,
}

impl WsGlobalRateLimitHandle {
    pub fn new(provider: WsProviderId, limiter: ActorRef<WsGlobalRateLimiterActor>) -> Self {
        Self {
            provider,
            limiter,
            batch: 1,
        }
    }

    pub fn with_batch(mut self, batch: u32) -> Self {
        self.batch = batch.max(1);
        self
    }
}

#[derive(Debug)]
struct ProviderState {
    limiter: WsRateLimiter,
}

impl ProviderState {
    fn new(cfg: WsRateLimitConfig) -> Self {
        Self {
            limiter: WsRateLimiter::new(cfg.max_per_window, cfg.window),
        }
    }
}

/// Actor implementing a global, lock-free, per-provider outbound rate limiter.
///
/// This is intended to gate *outbound* sends (subscriptions, auth, requests). Inbound
/// data must never be rate-limited.
pub struct WsGlobalRateLimiterActor {
    default: WsRateLimitConfig,
    providers: HashMap<WsProviderId, ProviderState>,
}

impl WsGlobalRateLimiterActor {
    pub fn new(default: WsRateLimitConfig) -> Self {
        Self {
            default,
            providers: HashMap::new(),
        }
    }

    fn provider_mut(&mut self, provider: &WsProviderId) -> &mut ProviderState {
        self.providers
            .entry(provider.clone())
            .or_insert_with(|| ProviderState::new(self.default))
    }
}

impl Actor for WsGlobalRateLimiterActor {
    type Args = Self;
    type Error = WebSocketError;

    fn name() -> &'static str {
        "WsGlobalRateLimiterActor"
    }

    async fn on_start(args: Self::Args, _ctx: ActorRef<Self>) -> WebSocketResult<Self> {
        Ok(args)
    }
}

/// Configure/override the rate limit for a provider id.
#[derive(Clone, Debug)]
pub struct SetProviderLimit {
    pub provider: WsProviderId,
    pub max_per_window: u32,
    pub window: Duration,
}

impl KameoMessage<SetProviderLimit> for WsGlobalRateLimiterActor {
    type Reply = WebSocketResult<()>;

    async fn handle(
        &mut self,
        msg: SetProviderLimit,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let cfg = WsRateLimitConfig::new(msg.max_per_window, msg.window);
        self.providers
            .insert(msg.provider, ProviderState::new(cfg));
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct AcquirePermits {
    pub provider: WsProviderId,
    pub permits: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcquiredPermits {
    pub permits: u32,
}

impl KameoMessage<AcquirePermits> for WsGlobalRateLimiterActor {
    type Reply = WebSocketResult<AcquiredPermits>;

    async fn handle(
        &mut self,
        msg: AcquirePermits,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let want = msg.permits.max(1);
        let state = self.provider_mut(&msg.provider);

        let mut acquired = 0u32;
        for _ in 0..want {
            match state.limiter.try_acquire() {
                Ok(()) => acquired += 1,
                Err(WebSocketError::RateLimited { retry_after, .. }) => {
                    return Err(WebSocketError::RateLimited {
                        message: "rate limit exceeded".to_string(),
                        retry_after,
                    });
                }
                Err(other) => return Err(other),
            }
        }

        Ok(AcquiredPermits { permits: acquired })
    }
}

/// Spawn a per-crate supervisor for global rate limiters.
pub fn spawn_global_rate_limiter_supervisor(
) -> ActorRef<TypedSupervisor<WsGlobalRateLimiterActor>> {
    TypedSupervisor::spawn(TypedSupervisor::new(
        "shared-ws-global-rate-limiter",
        None,
        |_name| -> ActorRef<WsGlobalRateLimiterActor> {
            unreachable!("WsGlobalRateLimiterActor restart unsupported; must be explicitly recreated")
        },
    ))
}

/// Spawn a global limiter and link it to an existing supervisor.
pub async fn spawn_global_rate_limiter_supervised_with(
    supervisor: &ActorRef<TypedSupervisor<WsGlobalRateLimiterActor>>,
    default: WsRateLimitConfig,
) -> ActorRef<WsGlobalRateLimiterActor> {
    let actor = WsGlobalRateLimiterActor::spawn(WsGlobalRateLimiterActor::new(default));
    actor.link(supervisor).await;
    actor
}

#[cfg(test)]
mod tests {
    use super::*;
    use kameo::error::SendError;
    use tokio::time::sleep;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_limiter_is_global_per_provider() {
        let sup = spawn_global_rate_limiter_supervisor();
        let limiter = spawn_global_rate_limiter_supervised_with(
            &sup,
            WsRateLimitConfig::new(2, Duration::from_millis(60)),
        )
        .await;

        let provider: WsProviderId = Arc::from("provider-a");

        // Two independent "clients" share the same limiter actor.
        let client1 = limiter.clone();
        let client2 = limiter.clone();

        // Acquire two permits across both clients: should succeed.
        let a = client1
            .ask(AcquirePermits {
                provider: provider.clone(),
                permits: 1,
            })
            .await
            .unwrap();
        assert_eq!(a.permits, 1);

        let b = client2
            .ask(AcquirePermits {
                provider: provider.clone(),
                permits: 1,
            })
            .await
            .unwrap();
        assert_eq!(b.permits, 1);

        // Third permit within the same window should fail, regardless of which client asks.
        let third = client1
            .ask(AcquirePermits {
                provider: provider.clone(),
                permits: 1,
            })
            .await;
        assert!(
            matches!(
                third,
                Err(SendError::HandlerError(WebSocketError::RateLimited { retry_after: Some(_), .. }))
            ),
            "expected global rate limit to apply across clients, got {third:?}"
        );

        sleep(Duration::from_millis(80)).await;

        let after = client2
            .ask(AcquirePermits {
                provider: provider.clone(),
                permits: 1,
            })
            .await
            .unwrap();
        assert_eq!(after.permits, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_limiter_is_scoped_by_provider() {
        let sup = spawn_global_rate_limiter_supervisor();
        let limiter = spawn_global_rate_limiter_supervised_with(
            &sup,
            WsRateLimitConfig::new(1, Duration::from_millis(80)),
        )
        .await;

        let a: WsProviderId = Arc::from("provider-a");
        let b: WsProviderId = Arc::from("provider-b");

        limiter
            .ask(AcquirePermits {
                provider: a.clone(),
                permits: 1,
            })
            .await
            .unwrap();

        // Provider B should still be allowed in the same window.
        limiter
            .ask(AcquirePermits {
                provider: b.clone(),
                permits: 1,
            })
            .await
            .unwrap();
    }
}
