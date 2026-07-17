use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use super::types::{
    ManagedWsServerDispatchError, ManagedWsServerShutdownError, ManagedWsServerStatus,
    ManagedWsServerStatusError, ManagedWsServerTerminalError,
};

/// State shared between the server task and every handle/connection.
pub(crate) struct ServerShared<C> {
    pub(crate) command_tx: broadcast::Sender<C>,
    pub(crate) status_tx: watch::Sender<ManagedWsServerStatus>,
    pub(crate) shutdown_tx: watch::Sender<bool>,
    /// Write-once: the first terminal cause is the authoritative one and is
    /// retained for the life of the handle.
    pub(crate) terminal: OnceLock<ManagedWsServerTerminalError>,
    pub(crate) active: Arc<AtomicUsize>,
    pub(crate) next_connection_id: AtomicU64,
}

impl<C> ServerShared<C> {
    /// Advance the observable status. Never overwrites a terminal end state:
    /// once terminal, no later transition can disguise the failure as normal.
    pub(crate) fn set_status(&self, status: ManagedWsServerStatus) {
        if self.terminal.get().is_some() && status != ManagedWsServerStatus::Terminal {
            return;
        }
        self.status_tx.send_replace(status);
    }

    /// Record the terminal cause *before* publishing `Terminal`, so any observer
    /// that reacts to the status can always read the cause.
    pub(crate) fn set_terminal(&self, error: ManagedWsServerTerminalError) {
        let _ = self.terminal.set(error);
        self.status_tx.send_replace(ManagedWsServerStatus::Terminal);
    }

    #[inline]
    pub(crate) fn next_connection_id(&self) -> u64 {
        self.next_connection_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// Owning handle to a managed WebSocket server.
///
/// The handle owns the server task: dropping it aborts the accept loop, which
/// releases the listener and every connection task. That abort-on-drop is the
/// final cleanup guarantee; prefer [`shutdown_with_timeout`] for a graceful
/// close.
///
/// [`shutdown_with_timeout`]: ManagedWsServerHandle::shutdown_with_timeout
pub struct ManagedWsServerHandle<C> {
    shared: Arc<ServerShared<C>>,
    task: Option<JoinHandle<()>>,
}

impl<C> ManagedWsServerHandle<C>
where
    C: Clone + Send + 'static,
{
    pub(crate) fn new(shared: Arc<ServerShared<C>>, task: JoinHandle<()>) -> Self {
        Self {
            shared,
            task: Some(task),
        }
    }

    /// Fan `command` out to every live connection.
    ///
    /// Returns the number of connections the command was queued to. Zero
    /// connections is a normal outcome, not an error. Only one command is
    /// retained per connection: a client that falls behind is closed and must
    /// reconnect for an authoritative snapshot rather than be served stale
    /// frames.
    ///
    /// `command` is cloned once per connection, so keep payloads
    /// reference-counted — fanout should copy refcounts, never bytes.
    pub fn dispatch(&self, command: C) -> Result<usize, ManagedWsServerDispatchError> {
        if let Some(error) = self.shared.terminal.get() {
            return Err(ManagedWsServerDispatchError::Terminated(error.clone()));
        }
        match self.status_snapshot() {
            ManagedWsServerStatus::ShuttingDown | ManagedWsServerStatus::Stopped => {
                return Err(ManagedWsServerDispatchError::Stopped);
            }
            ManagedWsServerStatus::Terminal => {
                // Terminal status is always published after the cause is stored,
                // so a re-read resolves the race above rather than guessing.
                match self.shared.terminal.get() {
                    Some(error) => {
                        return Err(ManagedWsServerDispatchError::Terminated(error.clone()));
                    }
                    None => return Err(ManagedWsServerDispatchError::Stopped),
                }
            }
            ManagedWsServerStatus::Starting
            | ManagedWsServerStatus::Retrying
            | ManagedWsServerStatus::Listening => {}
        }
        match self.shared.command_tx.send(command) {
            Ok(delivered) => Ok(delivered),
            // `send` only fails when there are no subscribers at all.
            Err(_) => Ok(0),
        }
    }

    /// Current lifecycle status.
    #[inline]
    pub fn status_snapshot(&self) -> ManagedWsServerStatus {
        *self.shared.status_tx.borrow()
    }

    /// Wait until the server reports `expected`.
    ///
    /// Fails fast rather than burning the timeout if the server has already
    /// settled in a different final state.
    pub async fn wait_for_status(
        &self,
        expected: ManagedWsServerStatus,
        timeout: Duration,
    ) -> Result<(), ManagedWsServerStatusError> {
        let mut status_rx = self.shared.status_tx.subscribe();
        let observed = tokio::time::timeout(timeout, async {
            loop {
                let current = *status_rx.borrow_and_update();
                if current == expected {
                    return Ok(());
                }
                if current.is_final() {
                    return Err(ManagedWsServerStatusError::Settled {
                        expected,
                        last: current,
                    });
                }
                if status_rx.changed().await.is_err() {
                    return Err(ManagedWsServerStatusError::Settled {
                        expected,
                        last: current,
                    });
                }
            }
        })
        .await;
        match observed {
            Ok(result) => result,
            Err(_) => Err(ManagedWsServerStatusError::Timeout {
                expected,
                last: self.status_snapshot(),
                timeout,
            }),
        }
    }

    /// Live connection count for this server instance.
    #[inline]
    pub fn active_connection_count(&self) -> usize {
        self.shared.active.load(Ordering::SeqCst)
    }

    /// The retained terminal cause, if the server failed unrecoverably.
    ///
    /// Always `None` for a normally stopped server: a clean shutdown can never
    /// present as a terminal failure.
    pub fn terminal_error(&self) -> Option<ManagedWsServerTerminalError> {
        self.shared.terminal.get().cloned()
    }

    /// Close the listener, drain clients, and stop the server task.
    ///
    /// On timeout the task is aborted — the listener and all connections are
    /// still released — and the timeout is reported.
    pub async fn shutdown_with_timeout(
        mut self,
        timeout: Duration,
    ) -> Result<(), ManagedWsServerShutdownError> {
        self.shared.shutdown_tx.send_replace(true);
        let Some(mut task) = self.task.take() else {
            return Ok(());
        };
        match tokio::time::timeout(timeout, &mut task).await {
            Ok(_) => Ok(()),
            Err(_) => {
                task.abort();
                Err(ManagedWsServerShutdownError::Timeout { timeout })
            }
        }
    }
}

impl<C> std::fmt::Debug for ManagedWsServerHandle<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedWsServerHandle")
            .field("status", &*self.shared.status_tx.borrow())
            .field(
                "active_connections",
                &self.shared.active.load(Ordering::SeqCst),
            )
            .field("terminal_error", &self.shared.terminal.get())
            .finish()
    }
}

impl<C> Drop for ManagedWsServerHandle<C> {
    fn drop(&mut self) {
        // Signal first so a task between select points observes the intent, then
        // abort: aborting the accept loop drops the listener and the connection
        // JoinSet, which aborts every client task with it. No transport outlives
        // its handle.
        self.shared.shutdown_tx.send_replace(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
