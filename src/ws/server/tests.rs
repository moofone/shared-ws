//! Unit-level coverage for the managed server contracts.
//!
//! These are the middle layer of the symmetric test set: the endpoint state
//! machine is driven by synthetic [`WsServerEndpointEvent`]s with no socket, and
//! the pure configuration/status/batch logic is exercised directly. The real
//! transport is covered by `tests/managed_ws_server.rs`.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;

use super::config::{WsServerConfig, WsServerRetryPolicy};
use super::types::{
    ManagedWsServerStartError, ManagedWsServerStatus, ManagedWsServerTerminalError,
    WS_SERVER_FRAME_BATCH_CAPACITY, WsServerCloseCause, WsServerConnectionContext,
    WsServerEndpointAction, WsServerEndpointEvent, WsServerEndpointHandler, WsServerFrameBatch,
};
use crate::core::frame::{WsFrame, frame_bytes};

/// A named mutation that should make a config/policy invalid.
type ConfigMutation = Box<dyn Fn(&mut WsServerConfig)>;
type RetryMutation = Box<dyn Fn(&mut WsServerRetryPolicy)>;

fn addr() -> SocketAddr {
    "127.0.0.1:8788".parse().expect("addr")
}

fn ctx() -> WsServerConnectionContext {
    WsServerConnectionContext {
        connection_id: 7,
        peer_addr: "127.0.0.1:51000".parse().expect("peer"),
        local_addr: addr(),
    }
}

// ---------------------------------------------------------------------------
// Configuration validation
// ---------------------------------------------------------------------------

#[test]
fn default_config_matches_the_documented_dashboard_deployment() {
    let config = WsServerConfig::new(addr());
    assert_eq!(config.max_connections, 256, "total cap, not per-IP");
    assert_eq!(config.backlog, 1024);
    assert_eq!(config.handshake_timeout, Duration::from_secs(5));
    assert_eq!(config.endpoint_tick_interval, Duration::from_secs(1));
    assert_eq!(config.shutdown_timeout, Duration::from_secs(5));
    assert_eq!(config.retained_commands_per_client, 1);
    assert!(config.validate().is_ok());
}

#[test]
fn default_retry_policy_starts_at_100ms_doubles_and_caps_at_five_seconds() {
    let retry = WsServerRetryPolicy::default();
    assert_eq!(retry.initial_backoff, Duration::from_millis(100));
    assert_eq!(retry.max_backoff, Duration::from_secs(5));
    assert_eq!(retry.max_attempts, None, "self-heals forever by default");
    assert!(retry.validate().is_ok());

    let mut backoff = retry.initial_backoff;
    let mut seen = vec![backoff];
    for _ in 0..8 {
        backoff = retry.next_backoff(backoff);
        seen.push(backoff);
    }
    assert_eq!(
        seen,
        vec![
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(400),
            Duration::from_millis(800),
            Duration::from_millis(1600),
            Duration::from_millis(3200),
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_secs(5),
        ],
        "backoff doubles then saturates at the cap"
    );
}

#[test]
fn invalid_config_is_rejected_field_by_field() {
    let cases: Vec<(&'static str, ConfigMutation)> = vec![
        ("backlog", Box::new(|c: &mut WsServerConfig| c.backlog = 0)),
        (
            "max_connections",
            Box::new(|c: &mut WsServerConfig| c.max_connections = 0),
        ),
        (
            "handshake_timeout",
            Box::new(|c: &mut WsServerConfig| c.handshake_timeout = Duration::ZERO),
        ),
        (
            "endpoint_tick_interval",
            Box::new(|c: &mut WsServerConfig| c.endpoint_tick_interval = Duration::ZERO),
        ),
        (
            "shutdown_timeout",
            Box::new(|c: &mut WsServerConfig| c.shutdown_timeout = Duration::ZERO),
        ),
        (
            "retained_commands_per_client",
            Box::new(|c: &mut WsServerConfig| c.retained_commands_per_client = 0),
        ),
    ];
    for (field, break_it) in cases {
        let mut config = WsServerConfig::new(addr());
        break_it(&mut config);
        match config.validate() {
            Err(ManagedWsServerStartError::InvalidConfig { field: got, .. }) => {
                assert_eq!(got, field, "wrong field reported")
            }
            other => panic!("{field} must be rejected, got {other:?}"),
        }
    }
}

#[test]
fn invalid_retry_policy_is_rejected_field_by_field() {
    let cases: Vec<(&'static str, RetryMutation)> = vec![
        (
            "initial_backoff",
            Box::new(|r: &mut WsServerRetryPolicy| r.initial_backoff = Duration::ZERO),
        ),
        (
            "max_backoff",
            Box::new(|r: &mut WsServerRetryPolicy| r.max_backoff = Duration::from_millis(1)),
        ),
        (
            "max_attempts",
            Box::new(|r: &mut WsServerRetryPolicy| r.max_attempts = Some(0)),
        ),
    ];
    for (field, break_it) in cases {
        let mut retry = WsServerRetryPolicy::default();
        break_it(&mut retry);
        match retry.validate() {
            Err(ManagedWsServerStartError::InvalidConfig { field: got, .. }) => {
                assert_eq!(got, field)
            }
            other => panic!("{field} must be rejected, got {other:?}"),
        }
    }
}

#[test]
fn retry_budget_is_exhausted_only_at_the_configured_attempt_count() {
    let mut retry = WsServerRetryPolicy {
        max_attempts: Some(3),
        ..WsServerRetryPolicy::default()
    };
    assert!(!retry.is_exhausted(1));
    assert!(!retry.is_exhausted(2));
    assert!(retry.is_exhausted(3));
    assert!(retry.is_exhausted(4));

    retry.max_attempts = None;
    assert!(
        !retry.is_exhausted(u32::MAX),
        "unbounded retry never exhausts"
    );
}

// ---------------------------------------------------------------------------
// Status semantics
// ---------------------------------------------------------------------------

#[test]
fn only_stopped_and_terminal_are_final_and_they_are_disjoint() {
    assert!(ManagedWsServerStatus::Stopped.is_final());
    assert!(ManagedWsServerStatus::Terminal.is_final());
    for transient in [
        ManagedWsServerStatus::Starting,
        ManagedWsServerStatus::Retrying,
        ManagedWsServerStatus::Listening,
        ManagedWsServerStatus::ShuttingDown,
    ] {
        assert!(!transient.is_final(), "{transient:?} must not be final");
    }
    assert_ne!(
        ManagedWsServerStatus::Stopped,
        ManagedWsServerStatus::Terminal,
        "a normal stop must never be indistinguishable from a terminal failure"
    );
}

#[test]
fn terminal_errors_describe_their_cause() {
    let bind = ManagedWsServerTerminalError::BindRetryExhausted {
        addr: addr(),
        attempts: 3,
        error: "address in use".to_string(),
    };
    let rendered = bind.to_string();
    assert!(rendered.contains("127.0.0.1:8788"));
    assert!(rendered.contains('3'));
    assert!(rendered.contains("address in use"));

    let internal = ManagedWsServerTerminalError::InternalLoopFailure {
        reason: "injected".to_string(),
    };
    assert!(internal.to_string().contains("injected"));
}

// ---------------------------------------------------------------------------
// Frame batch
// ---------------------------------------------------------------------------

#[test]
fn frame_batch_is_inline_bounded_and_never_silently_drops() {
    let mut batch = WsServerFrameBatch::new();
    assert!(batch.is_empty());
    for i in 0..WS_SERVER_FRAME_BATCH_CAPACITY {
        assert!(
            batch
                .push(WsFrame::Binary(Bytes::from(vec![i as u8])))
                .is_ok()
        );
    }
    assert_eq!(batch.len(), WS_SERVER_FRAME_BATCH_CAPACITY);
    assert!(!batch.is_empty());

    // Overflow returns the frame rather than dropping it.
    let overflow = WsFrame::Binary(Bytes::from_static(b"overflow"));
    match batch.push(overflow) {
        Err(returned) => assert_eq!(frame_bytes(&returned), Some(&b"overflow"[..])),
        Ok(()) => panic!("batch must refuse to exceed its inline capacity"),
    }

    let drained: Vec<_> = batch.drain().collect();
    assert_eq!(drained.len(), WS_SERVER_FRAME_BATCH_CAPACITY, "FIFO order");
    assert_eq!(frame_bytes(&drained[0]), Some(&[0u8][..]));
    assert_eq!(batch.len(), 0, "drain resets the batch");
    assert_eq!(batch.drain().count(), 0, "draining twice yields nothing");
}

#[test]
fn frame_batch_from_iter_stops_at_capacity() {
    let batch: WsServerFrameBatch = (0..16u8)
        .map(|i| WsFrame::Binary(Bytes::from(vec![i])))
        .collect();
    assert_eq!(batch.len(), WS_SERVER_FRAME_BATCH_CAPACITY);
}

// ---------------------------------------------------------------------------
// Endpoint state machine, driven by synthetic events (no transport)
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
#[error("synthetic endpoint rejected: {0}")]
struct SyntheticError(&'static str);

/// A miniature of the real dashboard endpoint: subscribe, then receive only
/// commands matching the subscribed target.
#[derive(Default)]
struct SyntheticEndpoint {
    subscribed: Option<String>,
    ticks: u32,
    closed_with: Option<WsServerCloseCause>,
}

#[derive(Clone, Debug)]
struct SyntheticCommand {
    target: Option<&'static str>,
    payload: Bytes,
}

impl WsServerEndpointHandler for SyntheticEndpoint {
    type Command = SyntheticCommand;
    type Error = SyntheticError;

    async fn on_event(
        &mut self,
        event: WsServerEndpointEvent<'_, Self::Command>,
    ) -> Result<WsServerEndpointAction, Self::Error> {
        match event {
            WsServerEndpointEvent::Connected(_) => Ok(WsServerEndpointAction::Send(
                WsFrame::text_static(r#"{"type":"hello"}"#),
            )),
            WsServerEndpointEvent::Frame(frame) => {
                let Some(bytes) = frame_bytes(frame) else {
                    return Err(SyntheticError("control frame reached the handler"));
                };
                if bytes.is_empty() {
                    return Err(SyntheticError("empty subscribe"));
                }
                self.subscribed = Some(String::from_utf8_lossy(bytes).into_owned());
                let mut batch = WsServerFrameBatch::new();
                let _ = batch.push(WsFrame::text_static(r#"{"type":"ack"}"#));
                let _ = batch.push(WsFrame::text_static(r#"{"type":"snapshot"}"#));
                Ok(WsServerEndpointAction::SendBatch(batch))
            }
            WsServerEndpointEvent::Command(command) => {
                let matches = match (command.target, self.subscribed.as_deref()) {
                    (None, _) => true,
                    (Some(target), Some(subscribed)) => target == subscribed,
                    (Some(_), None) => false,
                };
                if matches {
                    Ok(WsServerEndpointAction::Send(WsFrame::Binary(
                        command.payload.clone(),
                    )))
                } else {
                    Ok(WsServerEndpointAction::None)
                }
            }
            WsServerEndpointEvent::Tick => {
                self.ticks += 1;
                if self.ticks >= 5 && self.subscribed.is_none() {
                    // Application-level subscription deadline.
                    return Ok(WsServerEndpointAction::Close(None));
                }
                Ok(WsServerEndpointAction::None)
            }
            WsServerEndpointEvent::Closed(cause) => {
                self.closed_with = Some(cause);
                Ok(WsServerEndpointAction::None)
            }
        }
    }
}

#[tokio::test]
async fn synthetic_events_drive_the_endpoint_through_its_whole_lifecycle() {
    let mut endpoint = SyntheticEndpoint::default();
    let ctx = ctx();

    // Connected → greeting.
    let action = endpoint
        .on_event(WsServerEndpointEvent::Connected(&ctx))
        .await
        .expect("connected");
    match action {
        WsServerEndpointAction::Send(frame) => {
            assert_eq!(frame_bytes(&frame), Some(&br#"{"type":"hello"}"#[..]))
        }
        other => panic!("expected a greeting, got {other:?}"),
    }

    // Before subscribing, a targeted command is filtered out.
    let action = endpoint
        .on_event(WsServerEndpointEvent::Command(&SyntheticCommand {
            target: Some("alpha"),
            payload: Bytes::from_static(b"early"),
        }))
        .await
        .expect("command");
    assert!(matches!(action, WsServerEndpointAction::None));

    // Subscribe → batched ack + snapshot.
    let subscribe = WsFrame::text_static("alpha");
    let action = endpoint
        .on_event(WsServerEndpointEvent::Frame(&subscribe))
        .await
        .expect("subscribe");
    match action {
        WsServerEndpointAction::SendBatch(mut batch) => {
            let frames: Vec<_> = batch.drain().collect();
            assert_eq!(frames.len(), 2);
            assert_eq!(frame_bytes(&frames[0]), Some(&br#"{"type":"ack"}"#[..]));
        }
        other => panic!("expected a batch, got {other:?}"),
    }

    // A matching target is delivered; a non-matching one is not.
    let payload = Bytes::from_static(b"alpha-event");
    let action = endpoint
        .on_event(WsServerEndpointEvent::Command(&SyntheticCommand {
            target: Some("alpha"),
            payload: payload.clone(),
        }))
        .await
        .expect("command");
    match action {
        WsServerEndpointAction::Send(WsFrame::Binary(bytes)) => {
            assert_eq!(bytes, payload);
            assert_eq!(
                bytes.as_ptr(),
                payload.as_ptr(),
                "delivery clones a refcount, not the payload"
            );
        }
        other => panic!("expected the matching payload, got {other:?}"),
    }
    let action = endpoint
        .on_event(WsServerEndpointEvent::Command(&SyntheticCommand {
            target: Some("beta"),
            payload: Bytes::from_static(b"beta-event"),
        }))
        .await
        .expect("command");
    assert!(matches!(action, WsServerEndpointAction::None));

    // An untargeted command reaches every subscriber.
    let action = endpoint
        .on_event(WsServerEndpointEvent::Command(&SyntheticCommand {
            target: None,
            payload: Bytes::from_static(b"universal"),
        }))
        .await
        .expect("command");
    assert!(matches!(action, WsServerEndpointAction::Send(_)));

    // Closed is terminal and carries the cause.
    endpoint
        .on_event(WsServerEndpointEvent::Closed(WsServerCloseCause::Lagged {
            missed: 3,
        }))
        .await
        .expect("closed");
    assert_eq!(
        endpoint.closed_with,
        Some(WsServerCloseCause::Lagged { missed: 3 })
    );
}

#[tokio::test]
async fn an_endpoint_error_is_reported_to_the_caller_verbatim() {
    let mut endpoint = SyntheticEndpoint::default();
    let empty = WsFrame::Binary(Bytes::new());
    let error = endpoint
        .on_event(WsServerEndpointEvent::Frame(&empty))
        .await
        .expect_err("empty subscribe must be rejected");
    assert!(error.to_string().contains("empty subscribe"));
}

#[tokio::test]
async fn ticks_can_enforce_an_application_subscription_deadline() {
    let mut endpoint = SyntheticEndpoint::default();
    for _ in 0..4 {
        let action = endpoint
            .on_event(WsServerEndpointEvent::Tick)
            .await
            .expect("tick");
        assert!(matches!(action, WsServerEndpointAction::None));
    }
    // Fifth tick without a subscription closes the connection.
    let action = endpoint
        .on_event(WsServerEndpointEvent::Tick)
        .await
        .expect("tick");
    assert!(
        matches!(action, WsServerEndpointAction::Close(None)),
        "a silent client must be closed at the deadline"
    );
}

#[tokio::test]
async fn a_subscribed_endpoint_is_never_closed_by_the_tick_deadline() {
    let mut endpoint = SyntheticEndpoint::default();
    let subscribe = WsFrame::text_static("alpha");
    endpoint
        .on_event(WsServerEndpointEvent::Frame(&subscribe))
        .await
        .expect("subscribe");
    for _ in 0..10 {
        let action = endpoint
            .on_event(WsServerEndpointEvent::Tick)
            .await
            .expect("tick");
        assert!(matches!(action, WsServerEndpointAction::None));
    }
}
