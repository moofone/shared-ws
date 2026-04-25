use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use crate::transport::WsTransport;
use crate::ws::{
    WebSocketBufferConfig, WebSocketError, WsConnectionStatus, WsDisconnectAction,
    WsDisconnectCause, WsEndpointHandler, WsErrorAction, WsFrame, WsMessageAction, WsParseOutcome,
    WsReconnectStrategy, WsSessionMode, WsSubscriptionAction, WsSubscriptionManager,
};

#[derive(Debug)]
pub enum ManagedWsCommand<S = ()> {
    Send(Vec<u8>),
    UpdateSubscriptions(WsSubscriptionAction<S>),
    ForceReconnect(String),
    Shutdown,
}

#[derive(Debug)]
pub struct ManagedWsRuntime {
    pub cmd_tx: mpsc::UnboundedSender<ManagedWsCommand>,
    pub status: Arc<AtomicU8>,
}

#[inline]
pub fn ws_status_to_u8(status: WsConnectionStatus) -> u8 {
    match status {
        WsConnectionStatus::Connecting => 0,
        WsConnectionStatus::Connected => 1,
        WsConnectionStatus::Disconnected => 2,
    }
}

#[inline]
pub fn ws_status_from_u8(raw: u8) -> WsConnectionStatus {
    match raw {
        0 => WsConnectionStatus::Connecting,
        1 => WsConnectionStatus::Connected,
        _ => WsConnectionStatus::Disconnected,
    }
}

#[inline]
pub fn is_expected_ws_disconnect(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("close_notify")
        || normalized.contains("unexpected-eof")
        || normalized.contains("connection reset by peer")
        || normalized.contains("connection reset without closing handshake")
        || normalized.contains("connection closed")
        || normalized.contains("closed connection")
        || normalized.contains("broken pipe")
        || normalized.contains("trying to work with closed connection")
        || normalized.contains("already closed")
}

#[inline]
fn reconnect_delay<R: WsReconnectStrategy>(
    reconnect: &mut R,
    action: WsDisconnectAction,
) -> Option<Duration> {
    match action {
        WsDisconnectAction::Abort => None,
        WsDisconnectAction::ImmediateReconnect => Some(Duration::ZERO),
        WsDisconnectAction::BackoffReconnect => {
            if reconnect.should_retry() {
                Some(reconnect.next_delay())
            } else {
                None
            }
        }
    }
}

fn notify_disconnect<H: WsEndpointHandler>(
    handler: &mut H,
    cause: &WsDisconnectCause,
    will_reconnect: bool,
) -> Result<(), H::Error> {
    if let Some(message) = handler.on_connection_disconnected(cause, will_reconnect) {
        handler.handle_message(message)?;
    }
    Ok(())
}

async fn send_payload<H, W>(
    handler: &mut H,
    writer: &mut W,
    payload: Vec<u8>,
) -> Result<(), WebSocketError>
where
    H: WsEndpointHandler,
    W: futures_util::Sink<WsFrame, Error = WebSocketError> + Unpin,
{
    let frame = crate::ws::into_ws_message(payload);
    handler.on_outbound_frame(&frame);
    writer.send(frame).await
}

async fn replay_subscriptions<H, W>(handler: &mut H, writer: &mut W) -> Result<(), WebSocketError>
where
    H: WsEndpointHandler,
    W: futures_util::Sink<WsFrame, Error = WebSocketError> + Unpin,
{
    let subscriptions = handler.subscription_manager().initial_subscriptions();
    for subscription in subscriptions {
        let payload = handler
            .subscription_manager()
            .serialize_subscription(&subscription);
        send_payload(handler, writer, payload).await?;
    }
    Ok(())
}

async fn send_subscription_updates<H, W>(
    handler: &mut H,
    writer: &mut W,
    action: WsSubscriptionAction<
        <<H as WsEndpointHandler>::Subscription as WsSubscriptionManager>::SubscriptionMessage,
    >,
) -> Result<(), WebSocketError>
where
    H: WsEndpointHandler,
    W: futures_util::Sink<WsFrame, Error = WebSocketError> + Unpin,
{
    let subscriptions = handler
        .subscription_manager()
        .update_subscriptions(action)
        .map_err(WebSocketError::EndpointSpecific)?;
    for subscription in subscriptions {
        let payload = handler
            .subscription_manager()
            .serialize_subscription(&subscription);
        send_payload(handler, writer, payload).await?;
    }
    Ok(())
}

fn command_requires_auth<S>(command: &ManagedWsCommand<S>) -> bool {
    matches!(
        command,
        ManagedWsCommand::Send(_) | ManagedWsCommand::UpdateSubscriptions(_)
    )
}

pub async fn run_managed_ws_loop<T, H, R>(
    url: String,
    transport: T,
    mut handler: H,
    mut reconnect: R,
    status: Arc<AtomicU8>,
    mut cmd_rx: mpsc::UnboundedReceiver<
        ManagedWsCommand<
            <<H as WsEndpointHandler>::Subscription as WsSubscriptionManager>::SubscriptionMessage,
        >,
    >,
) where
    T: WsTransport,
    H: WsEndpointHandler,
    R: WsReconnectStrategy,
{
    let mut is_reconnect = false;
    let mut pending_auth_commands = VecDeque::new();

    loop {
        status.store(
            ws_status_to_u8(WsConnectionStatus::Connecting),
            Ordering::Relaxed,
        );

        let connect = transport
            .connect(url.clone(), WebSocketBufferConfig::default())
            .await;
        let (mut reader, mut writer) = match connect {
            Ok(parts) => parts,
            Err(err) => {
                status.store(
                    ws_status_to_u8(WsConnectionStatus::Disconnected),
                    Ordering::Relaxed,
                );
                let cause = WsDisconnectCause::InternalError {
                    context: "connect".to_string(),
                    error: err.to_string(),
                };
                let action = handler.classify_disconnect(&cause);
                let delay = reconnect_delay(&mut reconnect, action);
                let will_reconnect = delay.is_some();
                let _ = notify_disconnect(&mut handler, &cause, will_reconnect);
                let Some(delay) = delay else {
                    return;
                };
                if !wait_reconnect_delay_or_shutdown(delay, &mut cmd_rx).await {
                    return;
                }
                is_reconnect = true;
                continue;
            }
        };

        reconnect.reset();
        status.store(
            ws_status_to_u8(WsConnectionStatus::Connected),
            Ordering::Relaxed,
        );
        handler.reset_state();
        handler.on_open();
        if let Some(message) = handler.on_connection_opened(is_reconnect) {
            let _ = handler.handle_message(message);
        }
        if let Some(payload) = handler.generate_auth()
            && let Err(error) = send_payload(&mut handler, &mut writer, payload).await
        {
            let cause = WsDisconnectCause::InternalError {
                context: "auth_send".to_string(),
                error: error.to_string(),
            };
            let action = handler.classify_disconnect(&cause);
            let delay = reconnect_delay(&mut reconnect, action);
            let will_reconnect = delay.is_some();
            let _ = notify_disconnect(&mut handler, &cause, will_reconnect);
            status.store(
                ws_status_to_u8(WsConnectionStatus::Disconnected),
                Ordering::Relaxed,
            );
            let Some(delay) = delay else {
                return;
            };
            if !wait_reconnect_delay_or_shutdown(delay, &mut cmd_rx).await {
                return;
            }
            is_reconnect = true;
            continue;
        }
        let mut session_authenticated = handler.session_mode() == WsSessionMode::Public;
        if session_authenticated
            && let Err(error) = replay_subscriptions(&mut handler, &mut writer).await
        {
            let cause = WsDisconnectCause::InternalError {
                context: "subscription_replay".to_string(),
                error: error.to_string(),
            };
            let action = handler.classify_disconnect(&cause);
            let delay = reconnect_delay(&mut reconnect, action);
            let will_reconnect = delay.is_some();
            let _ = notify_disconnect(&mut handler, &cause, will_reconnect);
            status.store(
                ws_status_to_u8(WsConnectionStatus::Disconnected),
                Ordering::Relaxed,
            );
            let Some(delay) = delay else {
                return;
            };
            if !wait_reconnect_delay_or_shutdown(delay, &mut cmd_rx).await {
                return;
            }
            is_reconnect = true;
            continue;
        }

        let cause = loop {
            tokio::select! {
                command = cmd_rx.recv() => {
                    match command {
                        Some(ManagedWsCommand::Shutdown) | None => {
                            status.store(ws_status_to_u8(WsConnectionStatus::Disconnected), Ordering::Relaxed);
                            return;
                        }
                        Some(ManagedWsCommand::ForceReconnect(reason)) => {
                            break WsDisconnectCause::EndpointRequested { reason };
                        }
                        Some(command) => {
                            if !session_authenticated && command_requires_auth(&command) {
                                pending_auth_commands.push_back(command);
                                continue;
                            }
                            if let Err(error) = apply_command(&mut handler, &mut writer, command).await {
                                break WsDisconnectCause::InternalError {
                                    context: "command".to_string(),
                                    error: error.to_string(),
                                };
                            }
                        }
                    }
                }
                maybe_frame = reader.next() => {
                    match maybe_frame {
                        Some(Ok(frame)) => {
                            handler.on_inbound_frame(&frame);
                            match handler.parse_frame(&frame) {
                                Ok(WsParseOutcome::Message(action)) => {
                                    match action {
                                        WsMessageAction::Continue => {}
                                        WsMessageAction::SessionAuthenticated => {
                                            session_authenticated = true;
                                            if let Err(error) = replay_subscriptions(&mut handler, &mut writer).await {
                                                break WsDisconnectCause::InternalError {
                                                    context: "subscription_replay_after_auth".to_string(),
                                                    error: error.to_string(),
                                                };
                                            }
                                            if let Err(error) = apply_pending_auth_commands(
                                                &mut handler,
                                                &mut writer,
                                                &mut pending_auth_commands,
                                            )
                                            .await
                                            {
                                                break WsDisconnectCause::InternalError {
                                                    context: "pending_auth_command".to_string(),
                                                    error: error.to_string(),
                                                };
                                            }
                                        }
                                        WsMessageAction::Process(message) => {
                                            if let Err(error) = handler.handle_message(message) {
                                                break WsDisconnectCause::InternalError {
                                                    context: "handle_message".to_string(),
                                                    error: error.to_string(),
                                                };
                                            }
                                        }
                                        WsMessageAction::Reconnect(reason) => {
                                            break WsDisconnectCause::EndpointRequested { reason };
                                        }
                                        WsMessageAction::Shutdown => {
                                            status.store(ws_status_to_u8(WsConnectionStatus::Disconnected), Ordering::Relaxed);
                                            return;
                                        }
                                    }
                                }
                                Ok(WsParseOutcome::ServerError { code, message, data }) => {
                                    if matches!(handler.handle_server_error(code, &message, data), WsErrorAction::Reconnect | WsErrorAction::Fatal) {
                                        break WsDisconnectCause::ServerError { code, message };
                                    }
                                }
                                Err(_error) => {}
                            }
                        }
                        Some(Err(error)) => {
                            break WsDisconnectCause::ReadFailure {
                                error: error.to_string(),
                            };
                        }
                        None => {
                            break WsDisconnectCause::RemoteClosed;
                        }
                    }
                }
            }
        };

        status.store(
            ws_status_to_u8(WsConnectionStatus::Disconnected),
            Ordering::Relaxed,
        );
        let action = handler.classify_disconnect(&cause);
        let delay = reconnect_delay(&mut reconnect, action);
        let will_reconnect = delay.is_some();
        let _ = notify_disconnect(&mut handler, &cause, will_reconnect);
        let Some(delay) = delay else {
            return;
        };
        if !wait_reconnect_delay_or_shutdown(delay, &mut cmd_rx).await {
            return;
        }
        is_reconnect = true;
    }
}

async fn apply_command<H, W>(
    handler: &mut H,
    writer: &mut W,
    command: ManagedWsCommand<
        <<H as WsEndpointHandler>::Subscription as WsSubscriptionManager>::SubscriptionMessage,
    >,
) -> Result<(), WebSocketError>
where
    H: WsEndpointHandler,
    W: futures_util::Sink<WsFrame, Error = WebSocketError> + Unpin,
{
    match command {
        ManagedWsCommand::Send(payload) => send_payload(handler, writer, payload).await,
        ManagedWsCommand::UpdateSubscriptions(action) => {
            send_subscription_updates(handler, writer, action).await
        }
        ManagedWsCommand::ForceReconnect(_) | ManagedWsCommand::Shutdown => Ok(()),
    }
}

async fn apply_pending_auth_commands<H, W>(
    handler: &mut H,
    writer: &mut W,
    pending_auth_commands: &mut VecDeque<
        ManagedWsCommand<
            <<H as WsEndpointHandler>::Subscription as WsSubscriptionManager>::SubscriptionMessage,
        >,
    >,
) -> Result<(), WebSocketError>
where
    H: WsEndpointHandler,
    W: futures_util::Sink<WsFrame, Error = WebSocketError> + Unpin,
{
    while let Some(command) = pending_auth_commands.pop_front() {
        apply_command(handler, writer, command).await?;
    }
    Ok(())
}

async fn wait_reconnect_delay_or_shutdown<S>(
    delay: Duration,
    cmd_rx: &mut mpsc::UnboundedReceiver<ManagedWsCommand<S>>,
) -> bool
where
    S: Send + 'static,
{
    if delay.is_zero() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(delay) => true,
        command = cmd_rx.recv() => !matches!(command, Some(ManagedWsCommand::Shutdown) | None),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU8;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::testing::ReconnectableMockTransport;
    use crate::ws::{
        ManagedWsCommand, WsConnectionStatus, WsDisconnectAction, WsDisconnectCause,
        WsEndpointHandler, WsErrorAction, WsMessageAction, WsParseOutcome, WsReconnectStrategy,
        WsSubscriptionAction, WsSubscriptionManager, WsSubscriptionStatus, into_ws_message,
    };

    use super::{run_managed_ws_loop, ws_status_from_u8};

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Observed {
        Open(bool),
        Disconnected(bool),
        Message(&'static str),
    }

    #[derive(Clone)]
    struct FastReconnect;

    impl WsReconnectStrategy for FastReconnect {
        fn next_delay(&mut self) -> Duration {
            Duration::from_millis(5)
        }

        fn reset(&mut self) {}

        fn should_retry(&self) -> bool {
            true
        }
    }

    struct TestEndpoint {
        events: Arc<Mutex<Vec<Observed>>>,
        subscriptions: TestSubscriptions,
        auth_payload: Option<&'static str>,
        auth_gated: bool,
    }

    impl TestEndpoint {
        fn new(
            events: Arc<Mutex<Vec<Observed>>>,
            subscription_payload: Option<&'static str>,
        ) -> Self {
            Self {
                events,
                subscriptions: TestSubscriptions {
                    payload: subscription_payload,
                },
                auth_payload: None,
                auth_gated: false,
            }
        }

        fn auth_gated(events: Arc<Mutex<Vec<Observed>>>, auth_payload: &'static str) -> Self {
            Self {
                events,
                subscriptions: TestSubscriptions { payload: None },
                auth_payload: Some(auth_payload),
                auth_gated: true,
            }
        }
    }

    struct TestSubscriptions {
        payload: Option<&'static str>,
    }

    impl WsSubscriptionManager for TestSubscriptions {
        type SubscriptionMessage = &'static str;

        fn initial_subscriptions(&mut self) -> Vec<Self::SubscriptionMessage> {
            match self.payload {
                Some(payload) => vec![payload],
                None => Vec::new(),
            }
        }

        fn update_subscriptions(
            &mut self,
            action: WsSubscriptionAction<Self::SubscriptionMessage>,
        ) -> Result<Vec<Self::SubscriptionMessage>, String> {
            match action {
                WsSubscriptionAction::Add(items) | WsSubscriptionAction::Replace(items) => {
                    self.payload = items.last().copied();
                    Ok(items)
                }
                WsSubscriptionAction::Remove(_) | WsSubscriptionAction::Clear => {
                    self.payload = None;
                    Ok(Vec::new())
                }
            }
        }

        fn serialize_subscription(&mut self, msg: &Self::SubscriptionMessage) -> Vec<u8> {
            msg.as_bytes().to_vec()
        }

        fn handle_subscription_response(&mut self, _data: &[u8]) -> WsSubscriptionStatus {
            WsSubscriptionStatus::NotSubscriptionResponse
        }
    }

    impl WsEndpointHandler for TestEndpoint {
        type Message = Observed;
        type Error = std::io::Error;
        type Subscription = TestSubscriptions;

        fn subscription_manager(&mut self) -> &mut Self::Subscription {
            &mut self.subscriptions
        }

        fn generate_auth(&self) -> Option<Vec<u8>> {
            self.auth_payload.map(|payload| payload.as_bytes().to_vec())
        }

        fn parse(&mut self, data: &[u8]) -> Result<WsParseOutcome<Self::Message>, Self::Error> {
            if data == b"auth-ok" {
                return Ok(WsParseOutcome::Message(
                    WsMessageAction::SessionAuthenticated,
                ));
            }
            if data == b"reconnect" {
                return Ok(WsParseOutcome::Message(WsMessageAction::Reconnect(
                    "test reconnect".to_string(),
                )));
            }
            Ok(WsParseOutcome::Message(WsMessageAction::Process(
                Observed::Message("frame"),
            )))
        }

        fn handle_message(&mut self, msg: Self::Message) -> Result<(), Self::Error> {
            self.events.lock().expect("events lock").push(msg);
            Ok(())
        }

        fn handle_server_error(
            &mut self,
            _code: Option<i32>,
            _message: &str,
            _data: Option<sonic_rs::Value>,
        ) -> WsErrorAction {
            WsErrorAction::Reconnect
        }

        fn reset_state(&mut self) {}

        fn classify_disconnect(&self, _cause: &WsDisconnectCause) -> WsDisconnectAction {
            WsDisconnectAction::BackoffReconnect
        }

        fn session_mode(&self) -> crate::ws::WsSessionMode {
            if self.auth_gated {
                crate::ws::WsSessionMode::AuthGated
            } else {
                crate::ws::WsSessionMode::Public
            }
        }

        fn on_connection_opened(&mut self, is_reconnect: bool) -> Option<Self::Message> {
            Some(Observed::Open(is_reconnect))
        }

        fn on_connection_disconnected(
            &mut self,
            _cause: &WsDisconnectCause,
            will_reconnect: bool,
        ) -> Option<Self::Message> {
            Some(Observed::Disconnected(will_reconnect))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn managed_loop_reconnects_and_emits_lifecycle_events_after_socket_drop() {
        let (transport, mut server) = ReconnectableMockTransport::channel_pair();
        let status = Arc::new(AtomicU8::new(super::ws_status_to_u8(
            WsConnectionStatus::Disconnected,
        )));
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let handle = tokio::spawn(run_managed_ws_loop(
            "ws://example.invalid".to_string(),
            transport,
            TestEndpoint::new(Arc::clone(&events), None),
            FastReconnect,
            Arc::clone(&status),
            cmd_rx,
        ));

        let mut first = server
            .accept_connection_timeout(Duration::from_secs(1))
            .await
            .expect("first connection");
        first.drop_socket();
        let second = server
            .accept_connection_timeout(Duration::from_secs(1))
            .await
            .expect("second connection after reconnect");
        assert_eq!(second.conn_id(), 2);
        cmd_tx.send(ManagedWsCommand::Shutdown).expect("shutdown");
        handle.await.expect("managed loop join");

        let events = events.lock().expect("events lock").clone();
        assert_eq!(
            events,
            vec![
                Observed::Open(false),
                Observed::Disconnected(true),
                Observed::Open(true),
            ]
        );
        assert_eq!(
            ws_status_from_u8(status.load(std::sync::atomic::Ordering::Relaxed)),
            WsConnectionStatus::Disconnected
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn managed_loop_replays_public_subscriptions_on_reconnect() {
        let (transport, mut server) = ReconnectableMockTransport::channel_pair();
        let status = Arc::new(AtomicU8::new(super::ws_status_to_u8(
            WsConnectionStatus::Disconnected,
        )));
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let handle = tokio::spawn(run_managed_ws_loop(
            "ws://example.invalid".to_string(),
            transport,
            TestEndpoint::new(Arc::clone(&events), Some("SUB")),
            FastReconnect,
            Arc::clone(&status),
            cmd_rx,
        ));

        let mut first = server
            .accept_connection_timeout(Duration::from_secs(1))
            .await
            .expect("first connection");
        let first_sub = first
            .recv_outbound_timeout(Duration::from_secs(1))
            .await
            .expect("first subscription replay");
        assert_eq!(crate::ws::message_bytes(&first_sub), Some(&b"SUB"[..]));
        first.drop_socket();
        let mut second = server
            .accept_connection_timeout(Duration::from_secs(1))
            .await
            .expect("second connection after reconnect");
        let second_sub = second
            .recv_outbound_timeout(Duration::from_secs(1))
            .await
            .expect("second subscription replay");
        assert_eq!(crate::ws::message_bytes(&second_sub), Some(&b"SUB"[..]));
        cmd_tx.send(ManagedWsCommand::Shutdown).expect("shutdown");
        handle.await.expect("managed loop join");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn managed_loop_endpoint_requested_reconnect_restores_message_flow() {
        let (transport, mut server) = ReconnectableMockTransport::channel_pair();
        let status = Arc::new(AtomicU8::new(super::ws_status_to_u8(
            WsConnectionStatus::Disconnected,
        )));
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let handle = tokio::spawn(run_managed_ws_loop(
            "ws://example.invalid".to_string(),
            transport,
            TestEndpoint::new(Arc::clone(&events), None),
            FastReconnect,
            Arc::clone(&status),
            cmd_rx,
        ));

        let first = server
            .accept_connection_timeout(Duration::from_secs(1))
            .await
            .expect("first connection");
        first
            .send_inbound(into_ws_message("reconnect"))
            .expect("send reconnect");
        let second = server
            .accept_connection_timeout(Duration::from_secs(1))
            .await
            .expect("second connection after endpoint reconnect");
        second
            .send_inbound(into_ws_message("data"))
            .expect("send data");
        tokio::time::sleep(Duration::from_millis(25)).await;
        cmd_tx.send(ManagedWsCommand::Shutdown).expect("shutdown");
        handle.await.expect("managed loop join");

        let events = events.lock().expect("events lock").clone();
        assert!(
            events.contains(&Observed::Message("frame")),
            "message flow must resume after endpoint-requested reconnect: {events:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn managed_loop_sends_outbound_command_on_open_socket() {
        let (transport, mut server) = ReconnectableMockTransport::channel_pair();
        let status = Arc::new(AtomicU8::new(super::ws_status_to_u8(
            WsConnectionStatus::Disconnected,
        )));
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let handle = tokio::spawn(run_managed_ws_loop(
            "ws://example.invalid".to_string(),
            transport,
            TestEndpoint::new(Arc::clone(&events), None),
            FastReconnect,
            Arc::clone(&status),
            cmd_rx,
        ));

        let mut conn = server
            .accept_connection_timeout(Duration::from_secs(1))
            .await
            .expect("connection");
        cmd_tx
            .send(ManagedWsCommand::Send(b"RAW".to_vec()))
            .expect("send command");
        let outbound = conn
            .recv_outbound_timeout(Duration::from_secs(1))
            .await
            .expect("raw outbound");
        assert_eq!(crate::ws::message_bytes(&outbound), Some(&b"RAW"[..]));

        cmd_tx.send(ManagedWsCommand::Shutdown).expect("shutdown");
        handle.await.expect("managed loop join");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn managed_loop_applies_dynamic_subscription_update() {
        let (transport, mut server) = ReconnectableMockTransport::channel_pair();
        let status = Arc::new(AtomicU8::new(super::ws_status_to_u8(
            WsConnectionStatus::Disconnected,
        )));
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let handle = tokio::spawn(run_managed_ws_loop(
            "ws://example.invalid".to_string(),
            transport,
            TestEndpoint::new(Arc::clone(&events), None),
            FastReconnect,
            Arc::clone(&status),
            cmd_rx,
        ));

        let mut conn = server
            .accept_connection_timeout(Duration::from_secs(1))
            .await
            .expect("connection");
        cmd_tx
            .send(ManagedWsCommand::UpdateSubscriptions(
                WsSubscriptionAction::Add(vec!["SUB"]),
            ))
            .expect("subscription command");
        let outbound = conn
            .recv_outbound_timeout(Duration::from_secs(1))
            .await
            .expect("subscription outbound");
        assert_eq!(crate::ws::message_bytes(&outbound), Some(&b"SUB"[..]));

        cmd_tx.send(ManagedWsCommand::Shutdown).expect("shutdown");
        handle.await.expect("managed loop join");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn managed_loop_auth_gates_outbound_until_endpoint_confirms_auth() {
        let (transport, mut server) = ReconnectableMockTransport::channel_pair();
        let status = Arc::new(AtomicU8::new(super::ws_status_to_u8(
            WsConnectionStatus::Disconnected,
        )));
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let handle = tokio::spawn(run_managed_ws_loop(
            "ws://example.invalid".to_string(),
            transport,
            TestEndpoint::auth_gated(Arc::clone(&events), "AUTH"),
            FastReconnect,
            Arc::clone(&status),
            cmd_rx,
        ));

        let mut conn = server
            .accept_connection_timeout(Duration::from_secs(1))
            .await
            .expect("connection");
        let auth = conn
            .recv_outbound_timeout(Duration::from_secs(1))
            .await
            .expect("auth outbound");
        assert_eq!(crate::ws::message_bytes(&auth), Some(&b"AUTH"[..]));

        cmd_tx
            .send(ManagedWsCommand::Send(b"RAW".to_vec()))
            .expect("send command");
        cmd_tx
            .send(ManagedWsCommand::UpdateSubscriptions(
                WsSubscriptionAction::Add(vec!["SUB"]),
            ))
            .expect("subscription command");
        assert!(
            conn.recv_outbound_timeout(Duration::from_millis(50))
                .await
                .is_none(),
            "auth-gated session must not write queued commands before auth"
        );

        conn.send_inbound(into_ws_message("auth-ok"))
            .expect("auth-ok inbound");
        let raw = conn
            .recv_outbound_timeout(Duration::from_secs(1))
            .await
            .expect("raw outbound after auth");
        let sub = conn
            .recv_outbound_timeout(Duration::from_secs(1))
            .await
            .expect("subscription outbound after auth");
        assert_eq!(crate::ws::message_bytes(&raw), Some(&b"RAW"[..]));
        assert_eq!(crate::ws::message_bytes(&sub), Some(&b"SUB"[..]));

        cmd_tx.send(ManagedWsCommand::Shutdown).expect("shutdown");
        handle.await.expect("managed loop join");
    }
}
