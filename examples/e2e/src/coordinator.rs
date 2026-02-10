use std::time::{Duration, Instant};

use kameo::error::SendError;
use kameo::prelude::ActorRef;
use shared_rate_limiter::{Cost, Deny, Key, Outcome, Permit, RateLimiter};
use shared_ws::ws::{WebSocketActor, WsDelegatedError, WsDelegatedOk, WsDelegatedRequest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegatedOutcome<Ok, Err> {
    Confirmed(Ok),
    Unconfirmed(Err),
    NotDelivered(Err),
    Denied { retry_after: Duration },
    Other(Err),
}

pub struct DelegatedSendCoordinator<'a> {
    limiter: &'a mut RateLimiter,
    key: Key,
    started_at: Instant,
}

impl<'a> DelegatedSendCoordinator<'a> {
    pub fn new(limiter: &'a mut RateLimiter, key: Key, started_at: Instant) -> Self {
        Self {
            limiter,
            key,
            started_at,
        }
    }

    fn now(&self) -> Duration {
        self.started_at.elapsed()
    }

    fn try_acquire(&mut self, cost: Cost) -> Result<Permit, Deny> {
        let now = self.now();
        self.limiter.try_acquire(self.key, cost, now)
    }

    fn commit(&mut self, permit: Permit, outcome: Outcome) {
        let now = self.now();
        self.limiter.commit(permit, outcome, now);
    }

    fn refund(&mut self, permit: Permit) {
        let now = self.now();
        self.limiter.refund(permit, now);
    }

    pub async fn send_and_account<E, R, P, I, T>(
        &mut self,
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
        let permit = match self.try_acquire(cost) {
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
                self.commit(permit, outcome);
                if ok.confirmed {
                    DelegatedOutcome::Confirmed(ok)
                } else {
                    DelegatedOutcome::Unconfirmed(WsDelegatedError::unconfirmed(
                        ok.request_id,
                        "sent but not confirmed",
                    ))
                }
            }
            Err(SendError::HandlerError(err)) => {
                match &err {
                    WsDelegatedError::NotDelivered { .. } => {
                        self.refund(permit);
                        DelegatedOutcome::NotDelivered(err)
                    }
                    WsDelegatedError::Unconfirmed { .. } => {
                        self.commit(permit, Outcome::SentNoConfirm);
                        DelegatedOutcome::Unconfirmed(err)
                    }
                    WsDelegatedError::EndpointRejected { .. } => {
                        // Response observed; treat as confirmed for accounting.
                        self.commit(permit, Outcome::Confirmed);
                        DelegatedOutcome::Other(err)
                    }
                    WsDelegatedError::PayloadMismatch { .. }
                    | WsDelegatedError::TooManyPending { .. } => {
                        // Local pre-send error.
                        self.refund(permit);
                        DelegatedOutcome::Other(err)
                    }
                }
            }
            Err(SendError::ActorNotRunning(_))
            | Err(SendError::MissingConnection)
            | Err(SendError::ConnectionClosed)
            | Err(SendError::MailboxFull(_))
            | Err(SendError::Timeout(_)) => {
                // Actor wasn't able to accept/process the request, or no reply was received.
                self.refund(permit);
                DelegatedOutcome::NotDelivered(WsDelegatedError::not_delivered(
                    0,
                    "actor send failed",
                ))
            }
            Err(SendError::ActorStopped) => {
                self.refund(permit);
                DelegatedOutcome::NotDelivered(WsDelegatedError::not_delivered(0, "actor stopped"))
            }
        }
    }
}
