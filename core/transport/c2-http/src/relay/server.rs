//! Embeddable relay server — can be driven from Python via PyO3.
//!
//! Runs axum + tokio in a background thread. Provides synchronous
//! control methods (start, stop, register_upstream, etc.) that send
//! commands to the async runtime via channels.
//!
//! Includes a configurable idle sweeper that periodically evicts
//! upstream connections that have not been used recently. Evicted
//! upstreams are lazily reconnected on the next HTTP request (see
//! `router::try_reconnect`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use c2_ipc::IpcClient;
use c2_config::RelayConfig;
use crate::relay::background::spawn_background_tasks;
use crate::relay::peer::{PeerEnvelope, PeerMessage};
use crate::relay::router;
use crate::relay::state::RelayState;

/// Errors from the embeddable relay control API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayControlError {
    DuplicateRoute { name: String },
    Other(String),
}

impl std::fmt::Display for RelayControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayControlError::DuplicateRoute { name } => {
                write!(f, "Route name already registered: '{name}'")
            }
            RelayControlError::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RelayControlError {}

impl RelayControlError {
    pub fn to_c2_error(&self) -> c2_error::C2Error {
        match self {
            RelayControlError::DuplicateRoute { .. } => c2_error::C2Error::new(
                c2_error::ErrorCode::ResourceAlreadyRegistered,
                self.to_string(),
            ),
            RelayControlError::Other(message) => c2_error::C2Error::unknown(message.clone()),
        }
    }
}

impl From<String> for RelayControlError {
    fn from(message: String) -> Self {
        RelayControlError::Other(message)
    }
}

impl From<&str> for RelayControlError {
    fn from(message: &str) -> Self {
        RelayControlError::Other(message.to_string())
    }
}

/// Commands sent from the sync API to the async runtime.
enum Command {
    RegisterUpstream {
        name: String,
        address: String,
        reply: oneshot::Sender<Result<(), RelayControlError>>,
    },
    UnregisterUpstream {
        name: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    ListRoutes {
        reply: oneshot::Sender<Vec<(String, String)>>,
    },
    Stop {
        reply: oneshot::Sender<()>,
    },
}

/// Embeddable relay server with a synchronous control API.
#[allow(dead_code)]
pub struct RelayServer {
    cmd_tx: Option<mpsc::Sender<Command>>,
    thread: Option<std::thread::JoinHandle<()>>,
    state: Arc<RelayState>,
    cancel: CancellationToken,
}

impl RelayServer {
    /// Start the relay server on a background thread.
    pub fn start(config: RelayConfig) -> Result<Self, String> {
        let addr: SocketAddr = config.bind.parse()
            .map_err(|e| format!("Invalid bind address '{}': {e}", config.bind))?;

        let config = Arc::new(config);
        let disseminator: Arc<dyn crate::relay::disseminator::Disseminator> =
            Arc::new(crate::relay::disseminator::FullBroadcast::new());
        let state = Arc::new(RelayState::new(config.clone(), disseminator));

        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(64);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

        let idle_timeout_secs = config.idle_timeout_secs;
        let server_state = state.clone();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let thread = std::thread::Builder::new()
            .name("c2-relay".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create tokio runtime");

                rt.block_on(async move {
                    Self::run(addr, server_state, cmd_rx, ready_tx, idle_timeout_secs, cancel_clone).await;
                });
            })
            .map_err(|e| format!("Failed to spawn relay thread: {e}"))?;

        // Wait for the listener to be ready.
        ready_rx
            .blocking_recv()
            .map_err(|_| "Relay thread exited before ready".to_string())?
            .map_err(|e| format!("Relay failed to start: {e}"))?;

        Ok(Self {
            cmd_tx: Some(cmd_tx),
            thread: Some(thread),
            state,
            cancel,
        })
    }

    /// Register a new upstream IPC connection.
    pub fn register_upstream(&self, name: &str, address: &str) -> Result<(), RelayControlError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send_cmd(Command::RegisterUpstream {
            name: name.to_string(),
            address: address.to_string(),
            reply: reply_tx,
        })
        .map_err(RelayControlError::Other)?;
        reply_rx
            .blocking_recv()
            .map_err(|_| RelayControlError::Other("Relay thread dropped".to_string()))?
    }

    /// Unregister an upstream by name.
    pub fn unregister_upstream(&self, name: &str) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send_cmd(Command::UnregisterUpstream {
            name: name.to_string(),
            reply: reply_tx,
        })?;
        reply_rx
            .blocking_recv()
            .map_err(|_| "Relay thread dropped".to_string())?
    }

    /// List registered routes: Vec<(name, address)>.
    pub fn list_routes(&self) -> Result<Vec<(String, String)>, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send_cmd(Command::ListRoutes { reply: reply_tx })?;
        reply_rx
            .blocking_recv()
            .map_err(|_| "Relay thread dropped".to_string())
    }

    /// Gracefully stop the relay server.
    pub fn stop(&mut self) -> Result<(), String> {
        // Leave broadcast happens inside the tokio runtime (in `run()`'s shutdown block).
        self.cancel.cancel();

        if let Some(tx) = self.cmd_tx.take() {
            let (reply_tx, reply_rx) = oneshot::channel();
            let _ = tx.blocking_send(Command::Stop { reply: reply_tx });
            let _ = reply_rx.blocking_recv();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        Ok(())
    }

    // -- Internal ---------------------------------------------------------

    fn send_cmd(&self, cmd: Command) -> Result<(), String> {
        self.cmd_tx
            .as_ref()
            .ok_or_else(|| "Relay is stopped".to_string())?
            .blocking_send(cmd)
            .map_err(|_| "Relay thread is not running".to_string())
    }

    async fn run(
        addr: SocketAddr,
        state: Arc<RelayState>,
        mut cmd_rx: mpsc::Receiver<Command>,
        ready_tx: oneshot::Sender<Result<(), String>>,
        idle_timeout_secs: u64,
        cancel: CancellationToken,
    ) {
        let app = router::build_router(state.clone());

        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => {
                let _ = ready_tx.send(Ok(()));
                l
            }
            Err(e) => {
                let _ = ready_tx.send(Err(format!("Failed to bind {addr}: {e}")));
                return;
            }
        };

        // Spawn background tasks (heartbeat, failure detection, anti-entropy, etc.)
        let bg_handles = spawn_background_tasks(state.clone(), cancel.clone());

        // Seed bootstrap (one-shot, non-blocking)
        if !state.config().seeds.is_empty() {
            let s = state.clone();
            tokio::spawn(async move {
                let client = crate::relay_client_builder()
                    .timeout(std::time::Duration::from_secs(5))
            .build()
                    .expect("c-two: failed to build reqwest Client for relay traffic");
                for seed_url in &s.config().seeds {
                    let join_url = format!("{seed_url}/_peer/join");
                    let envelope = PeerEnvelope::new(
                        s.relay_id(),
                        PeerMessage::RelayJoin {
                            relay_id: s.relay_id().to_string(),
                            url: s.config().effective_advertise_url(),
                        },
                    );
                    if let Ok(resp) = client.post(&join_url).json(&envelope).send().await {
                        if let Ok(snapshot) = resp.json::<crate::relay::types::FullSync>().await {
                            s.merge_snapshot(snapshot);
                            let peers = s.list_peers();
                            let announce = PeerEnvelope::new(
                                s.relay_id(),
                                PeerMessage::RelayJoin {
                                    relay_id: s.relay_id().to_string(),
                                    url: s.config().effective_advertise_url(),
                                },
                            );
                            s.disseminator().broadcast(announce, &peers);
                            break;
                        }
                    }
                }
            });
        }

        let server = axum::serve(listener, app);
        let sweeper = Self::idle_sweeper(state.clone(), idle_timeout_secs);

        tokio::select! {
            _ = server => {},
            _ = Self::command_loop(state.clone(), &mut cmd_rx) => {},
            _ = sweeper => {},
        }

        // Shutdown: broadcast leave, cancel background tasks
        let leave = PeerEnvelope::new(
            state.relay_id(),
            PeerMessage::RelayLeave { relay_id: state.relay_id().to_string() },
        );
        let peers = state.list_peers();
        let leave_handle = state.disseminator().broadcast(leave, &peers);
        if let Some(h) = leave_handle {
            let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
        }
        cancel.cancel();
        for handle in bg_handles {
            let _ = handle.await;
        }
    }

    /// Periodically evict upstream connections that have been idle
    /// longer than `idle_timeout_secs`.
    async fn idle_sweeper(state: Arc<RelayState>, idle_timeout_secs: u64) {
        // When idle_timeout is 0, still sweep for dead connections every 30s.
        let check_interval = if idle_timeout_secs == 0 {
            30
        } else {
            std::cmp::max(idle_timeout_secs / 10, 5)
        };
        let mut interval = tokio::time::interval(Duration::from_secs(check_interval));
        // idle_timeout_ms = u64::MAX means time-based eviction never fires,
        // but is_connected() checks still catch dead connections.
        let idle_timeout_ms = if idle_timeout_secs == 0 {
            u64::MAX
        } else {
            idle_timeout_secs * 1000
        };

        loop {
            interval.tick().await;

            let evicted = state.evict_idle(idle_timeout_ms);
            for (name, old_client) in evicted {
                if let Some(arc_client) = old_client {
                    let dead = !arc_client.is_connected();
                    tokio::spawn(async move {
                        let mut client = match Arc::try_unwrap(arc_client) {
                            Ok(c) => c,
                            Err(_arc) => return,
                        };
                        client.close().await;
                    });
                    if dead {
                        eprintln!("[relay] Evicted dead upstream: {name}");
                    } else {
                        eprintln!("[relay] Evicted idle upstream: {name}");
                    }
                }
            }
        }
    }

    async fn command_loop(
        state: Arc<RelayState>,
        cmd_rx: &mut mpsc::Receiver<Command>,
    ) {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                Command::RegisterUpstream { name, address, reply } => {
                    if state.has_local_route(&name) {
                        let _ = reply.send(Err(RelayControlError::DuplicateRoute { name }));
                        continue;
                    }
                    let mut client = IpcClient::new(&address);
                    let result = match client.connect().await {
                        Ok(()) => {
                            state.register_upstream(name, address, String::new(), String::new(), Arc::new(client));
                            Ok(())
                        }
                        Err(e) => Err(RelayControlError::Other(format!("Failed to connect: {e}"))),
                    };
                    let _ = reply.send(result);
                }
                Command::UnregisterUpstream { name, reply } => {
                    match state.unregister_upstream(&name) {
                        Some((_entry, old_client)) => {
                            if let Some(arc_client) = old_client {
                                tokio::spawn(async move {
                                    let mut client = match Arc::try_unwrap(arc_client) {
                                        Ok(c) => c,
                                        Err(_) => return,
                                    };
                                    client.close().await;
                                });
                            }
                            let _ = reply.send(Ok(()));
                        }
                        None => {
                            let _ = reply.send(Err(format!("Route name not registered: '{name}'")));
                        }
                    }
                }
                Command::ListRoutes { reply } => {
                    let routes: Vec<(String, String)> = state.list_routes()
                        .into_iter()
                        .map(|r| (r.name, r.ipc_address.unwrap_or_default()))
                        .collect();
                    let _ = reply.send(routes);
                }
                Command::Stop { reply } => {
                    let _ = reply.send(());
                    return;
                }
            }
        }
    }
}

impl Drop for RelayServer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::RelayControlError;

    #[test]
    fn duplicate_route_error_has_stable_variant_and_message() {
        let err = RelayControlError::DuplicateRoute {
            name: "grid".to_string(),
        };

        assert_eq!(
            err,
            RelayControlError::DuplicateRoute {
                name: "grid".to_string(),
            },
        );
        assert_eq!(err.to_string(), "Route name already registered: 'grid'");
    }

    #[test]
    fn duplicate_route_error_maps_to_c2_error() {
        let err = RelayControlError::DuplicateRoute {
            name: "grid".to_string(),
        };
        let c2 = err.to_c2_error();

        assert_eq!(c2.code, c2_error::ErrorCode::ResourceAlreadyRegistered);
        assert_eq!(c2.message, "Route name already registered: 'grid'");
    }
}
