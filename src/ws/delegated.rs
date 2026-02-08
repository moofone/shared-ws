use std::time::{Duration, Instant};

use crate::core::frame::WsFrame;
use crate::core::types::WsRateLimitFeedback;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsConfirmMode {
    /// Reply once the writer accepts the frame for writing.
    Sent,
    /// Reply only once an endpoint-specific confirmation is observed.
    ///
    /// Sprint 1 note: confirmation matching is not yet implemented, so this will time out as
    /// `Unconfirmed` unless the actor is extended (Sprint 2).
    Confirmed,
}

#[derive(Clone, Debug)]
pub struct WsDelegatedRequest {
    pub request_id: u64,
    pub fingerprint: u64,
    pub frame: WsFrame,
    pub confirm_deadline: Instant,
    pub confirm_mode: WsConfirmMode,
}

impl WsDelegatedRequest {
    #[inline]
    pub fn sent(
        request_id: u64,
        fingerprint: u64,
        frame: WsFrame,
        confirm_deadline: Instant,
    ) -> Self {
        Self {
            request_id,
            fingerprint,
            frame,
            confirm_deadline,
            confirm_mode: WsConfirmMode::Sent,
        }
    }

    #[inline]
    pub fn confirmed(
        request_id: u64,
        fingerprint: u64,
        frame: WsFrame,
        confirm_deadline: Instant,
    ) -> Self {
        Self {
            request_id,
            fingerprint,
            frame,
            confirm_deadline,
            confirm_mode: WsConfirmMode::Confirmed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsDelegatedOk {
    pub request_id: u64,
    pub confirmed: bool,
    pub rate_limit_feedback: Option<WsRateLimitFeedback>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WsDelegatedError {
    NotDelivered {
        request_id: u64,
        message: String,
    },
    Unconfirmed {
        request_id: u64,
        message: String,
    },
    PayloadMismatch {
        request_id: u64,
    },
    TooManyPending {
        max: usize,
    },
    EndpointRejected {
        request_id: u64,
        message: String,
        rate_limit_feedback: Option<WsRateLimitFeedback>,
    },
}

impl WsDelegatedError {
    #[inline]
    pub fn not_delivered(request_id: u64, message: impl Into<String>) -> Self {
        Self::NotDelivered {
            request_id,
            message: message.into(),
        }
    }

    #[inline]
    pub fn unconfirmed(request_id: u64, message: impl Into<String>) -> Self {
        Self::Unconfirmed {
            request_id,
            message: message.into(),
        }
    }

    #[inline]
    pub fn endpoint_rejected(
        request_id: u64,
        message: impl Into<String>,
        rate_limit_feedback: Option<WsRateLimitFeedback>,
    ) -> Self {
        Self::EndpointRejected {
            request_id,
            message: message.into(),
            rate_limit_feedback,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct WsDelegatedTimeout {
    pub deadline: Instant,
}

impl WsDelegatedTimeout {
    #[inline]
    pub fn after(d: Duration) -> Self {
        Self {
            deadline: Instant::now() + d,
        }
    }
}
