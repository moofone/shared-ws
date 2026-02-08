use std::time::{Duration, Instant};

use kameo::error::SendError;
use kameo::prelude::ActorRef;
use shared_rate_limiter::{Cost, Deny, Key, Outcome, Permit, RateLimiter};
use shared_ws::ws::{WebSocketActor, WsDelegatedError, WsDelegatedOk, WsDelegatedRequest};
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegatedOutcome<Ok, Err> {
    Confirmed(Ok),
    Unconfirmed(Err),
    NotDelivered(Err),
    Denied { retry_after: Duration },
    Other(Err),
}

pub struct DelegatedSendCoordinator<'a> {
    limiter: &'a Mutex<RateLimiter>,
    key: Key,
    started_at: Instant,
}

impl<'a> DelegatedSendCoordinator<'a> {
    pub fn new(limiter: &'a Mutex<RateLimiter>, key: Key) -> Self {
        Self {
            limiter,
            key,
            started_at: Instant::now(),
        }
    }

    fn now(&self) -> Duration {
        self.started_at.elapsed()
    }

    async fn try_acquire(&self, cost: Cost) -> Result<Permit, Deny> {
        let now = self.now();
        let mut limiter = self.limiter.lock().await;
        limiter.try_acquire(self.key, cost, now)
    }

    async fn commit(&self, permit: Permit, outcome: Outcome) {
        let now = self.now();
        let mut limiter = self.limiter.lock().await;
        limiter.commit(permit, outcome, now);
    }

    async fn refund(&self, permit: Permit) {
        let now = self.now();
        let mut limiter = self.limiter.lock().await;
        limiter.refund(permit, now);
    }

    pub async fn send_and_account<E, R, P, I, T>(
        &self,
        ws: &ActorRef<WebSocketActor<E, R, P, I, T>>,
        req: WsDelegatedRequest,
        cost: Cost,
    ) -> DelegatedOutcome<WsDelegatedOk, WsDelegatedError>
    where
        E: shared_ws::ws::WsEndpointHandler,
        R: shared_ws::ws::WsReconnectStrategy,
        P: shared_ws::ws::WsPingPongStrategy,
        I: shared_ws::ws::WsIngress<Message = E::Message>,
        T: shared_ws::transport::WsTransport,
    {
        let permit = match self.try_acquire(cost).await {
            Ok(p) => p,
            Err(deny) => {
                return DelegatedOutcome::Denied {
                    retry_after: deny.retry_after,
                };
            }
        };

        let res = ws.ask(req).await;
        match res {
            Ok(ok) => {
                let outcome = if ok.confirmed {
                    Outcome::Confirmed
                } else {
                    Outcome::SentNoConfirm
                };
                self.commit(permit, outcome).await;
                DelegatedOutcome::Confirmed(ok)
            }
            Err(SendError::HandlerError(err)) => {
                match &err {
                    WsDelegatedError::NotDelivered { .. } => {
                        self.refund(permit).await;
                        DelegatedOutcome::NotDelivered(err)
                    }
                    WsDelegatedError::Unconfirmed { .. } => {
                        self.commit(permit, Outcome::SentNoConfirm).await;
                        DelegatedOutcome::Unconfirmed(err)
                    }
                    WsDelegatedError::EndpointRejected { .. } => {
                        // Response observed; treat as confirmed for accounting.
                        self.commit(permit, Outcome::Confirmed).await;
                        DelegatedOutcome::Other(err)
                    }
                    WsDelegatedError::PayloadMismatch { .. }
                    | WsDelegatedError::TooManyPending { .. } => {
                        // Local pre-send error.
                        self.refund(permit).await;
                        DelegatedOutcome::Other(err)
                    }
                }
            }
            Err(SendError::ActorNotRunning(_))
            | Err(SendError::MailboxFull(_))
            | Err(SendError::MissingConnection)
            | Err(SendError::ConnectionClosed)
            | Err(SendError::Timeout(_)) => {
                // Actor wasn't able to accept/process the request, or no reply was received.
                self.refund(permit).await;
                DelegatedOutcome::NotDelivered(WsDelegatedError::not_delivered(
                    0,
                    "actor send failed",
                ))
            }
            Err(SendError::ActorStopped) => {
                self.refund(permit).await;
                DelegatedOutcome::NotDelivered(WsDelegatedError::not_delivered(0, "actor stopped"))
            }
        }
    }
}
