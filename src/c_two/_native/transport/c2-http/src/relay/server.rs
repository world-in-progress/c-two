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
use parking_lot::RwLock;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use c2_ipc::IpcClient;
use crate::relay::router;
use crate::relay::state::{RelayState, UpstreamPool};

/// Commands sent from the sync API to the async runtime.
enum Command {
    RegisterUpstream {
        name: String,
        address: String,
        reply: oneshot::Sender<Result<(), String>>,
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
pub struct RelayServer {
    cmd_tx: Option<mpsc::Sender<Command>>,
    thread: Option<std::thread::JoinHandle<()>>,
    _pool: Arc<RwLock<UpstreamPool>>,
}

impl RelayServer {
    /// Start the relay server on a background thread.
    ///
    /// `idle_timeout_secs` controls how long an upstream can be idle
    /// before the sweeper evicts its connection. Set to `0` to disable.
    pub fn start(bind: &str, idle_timeout_secs: u64) -> Result<Self, String> {
        let addr: SocketAddr = bind
            .parse()
            .map_err(|e| format!("Invalid bind address '{bind}': {e}"))?;

        let pool = Arc::new(RwLock::new(UpstreamPool::new()));
        let state = RelayState { pool: pool.clone() };

        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(64);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

        let thread = std::thread::Builder::new()
            .name("c2-relay".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create tokio runtime");

                rt.block_on(async move {
                    Self::run(addr, state, cmd_rx, ready_tx, idle_timeout_secs).await;
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
            _pool: pool,
        })
    }

    /// Register a new upstream IPC connection.
    pub fn register_upstream(&self, name: &str, address: &str) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send_cmd(Command::RegisterUpstream {
            name: name.to_string(),
            address: address.to_string(),
            reply: reply_tx,
        })?;
        reply_rx
            .blocking_recv()
            .map_err(|_| "Relay thread dropped".to_string())?
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
        state: RelayState,
        mut cmd_rx: mpsc::Receiver<Command>,
        ready_tx: oneshot::Sender<Result<(), String>>,
        idle_timeout_secs: u64,
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

        let server = axum::serve(listener, app);
        let sweeper = Self::idle_sweeper(state.clone(), idle_timeout_secs);

        // Run the HTTP server, command loop, and idle sweeper concurrently.
        tokio::select! {
            _ = server => {},
            _ = Self::command_loop(state, &mut cmd_rx) => {},
            _ = sweeper => {},
        }
    }

    /// Periodically evict upstream connections that have been idle
    /// longer than `idle_timeout_secs`.
    async fn idle_sweeper(state: RelayState, idle_timeout_secs: u64) {
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

            let idle_names = {
                let pool = state.pool.read();
                pool.idle_entries(idle_timeout_ms)
            };

            if idle_names.is_empty() {
                continue;
            }

            let mut pool = state.pool.write();
            for name in &idle_names {
                if let Some(old_client) = pool.evict(name) {
                    let dead = !old_client.is_connected();
                    tokio::spawn(async move {
                        let mut client = match Arc::try_unwrap(old_client) {
                            Ok(c) => c,
                            Err(_arc) => return, // still referenced elsewhere
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
        state: RelayState,
        cmd_rx: &mut mpsc::Receiver<Command>,
    ) {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                Command::RegisterUpstream { name, address, reply } => {
                    // Check duplicate without holding lock
                    {
                        let pool = state.pool.read();
                        if pool.contains(&name) {
                            let _ = reply.send(Err(format!("Route name already registered: '{name}'")));
                            continue;
                        }
                    }
                    // Connect without holding lock
                    let mut client = IpcClient::new(&address);
                    let result = match client.connect().await {
                        Ok(()) => {
                            let mut pool = state.pool.write();
                            pool.insert(name, address, Arc::new(client))
                        }
                        Err(e) => Err(format!("Failed to connect: {e}")),
                    };
                    let _ = reply.send(result);
                }
                Command::UnregisterUpstream { name, reply } => {
                    let result = {
                        let mut pool = state.pool.write();
                        pool.remove(&name)
                    };
                    match result {
                        Ok(old_client) => {
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
                        Err(e) => {
                            let _ = reply.send(Err(e));
                        }
                    }
                }
                Command::ListRoutes { reply } => {
                    let pool = state.pool.read();
                    let routes: Vec<(String, String)> = pool
                        .list_routes()
                        .into_iter()
                        .map(|r| (r.name, r.address))
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
