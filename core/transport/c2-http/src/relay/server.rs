//! Relay server runtime used by the `c3 relay` CLI and Rust callers.
//!
//! Runs axum + tokio in a background thread. Provides synchronous
//! control methods (start, stop, register_upstream, etc.) that send
//! commands to the async runtime via channels.
//!
//! Includes a configurable idle sweeper that periodically evicts
//! upstream connections that have not been used recently. Evicted
//! upstreams are lazily reconnected through the route's upstream slot
//! on the next HTTP request.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::relay::authority::{ControlError, RouteAuthority};
use crate::relay::background::spawn_background_tasks;
use crate::relay::gossip::{broadcast_route_announce, broadcast_route_withdraw};
use crate::relay::peer::{PeerEnvelope, PeerMessage};
use crate::relay::router;
use crate::relay::state::{RegisterCommitResult, RelayState, UnregisterResult};
use crate::relay::url::peer_endpoint_url;
use c2_config::RelayConfig;
use c2_ipc::{ClientIpcConfig, IpcClient};

/// Errors from the relay control API.
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
        server_id: String,
        address: String,
        reply: oneshot::Sender<Result<(), RelayControlError>>,
    },
    UnregisterUpstream {
        name: String,
        server_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    ListRoutes {
        reply: oneshot::Sender<Vec<(String, String)>>,
    },
    Stop {
        reply: oneshot::Sender<()>,
    },
}

fn validate_register_command(
    state: &RelayState,
    name: &str,
    server_id: &str,
) -> Result<(), RelayControlError> {
    let authority = RouteAuthority::new(state);
    authority
        .validate_route_name(name)
        .map_err(control_error_to_relay_error)?;
    authority
        .validate_server_id(server_id)
        .map_err(control_error_to_relay_error)
}

fn control_error_to_relay_error(err: ControlError) -> RelayControlError {
    match err {
        ControlError::InvalidName { reason }
        | ControlError::InvalidServerId { reason }
        | ControlError::InvalidServerInstanceId { reason }
        | ControlError::InvalidAddress { reason }
        | ControlError::ContractMismatch { reason } => RelayControlError::Other(reason),
        ControlError::AddressMismatch { .. }
        | ControlError::DuplicateRoute { .. }
        | ControlError::OwnerMismatch
        | ControlError::NotFound => {
            RelayControlError::Other("invalid relay register command".to_string())
        }
    }
}

fn close_client(client: IpcClient) {
    tokio::spawn(async move {
        let mut client = client;
        client.close().await;
    });
}

/// Relay server with a synchronous control API.
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
        config
            .validate()
            .map_err(|e| format!("Invalid relay config: {e}"))?;

        let addr: SocketAddr = config
            .bind
            .parse()
            .map_err(|e| format!("Invalid bind address '{}': {e}", config.bind))?;
        let effective_advertise_url = config.effective_advertise_url();
        if !crate::relay::route_table::valid_relay_url(&effective_advertise_url) {
            return Err(format!(
                "Invalid relay advertise_url '{}': must be an http(s) URL with a host",
                effective_advertise_url
            ));
        }

        let config = Arc::new(config);
        let disseminator: Arc<dyn crate::relay::disseminator::Disseminator> = Arc::new(
            crate::relay::disseminator::FullBroadcast::with_proxy_policy(config.use_proxy),
        );
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
                    Self::run(
                        addr,
                        server_state,
                        cmd_rx,
                        ready_tx,
                        idle_timeout_secs,
                        cancel_clone,
                    )
                    .await;
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

    /// Register a new upstream IPC connection with an explicit server identity.
    pub fn register_upstream(
        &self,
        name: &str,
        server_id: &str,
        address: &str,
    ) -> Result<(), RelayControlError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send_cmd(Command::RegisterUpstream {
            name: name.to_string(),
            server_id: server_id.to_string(),
            address: address.to_string(),
            reply: reply_tx,
        })
        .map_err(RelayControlError::Other)?;
        reply_rx
            .blocking_recv()
            .map_err(|_| RelayControlError::Other("Relay thread dropped".to_string()))?
    }

    /// Unregister an upstream by name.
    pub fn unregister_upstream(&self, name: &str, server_id: &str) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send_cmd(Command::UnregisterUpstream {
            name: name.to_string(),
            server_id: server_id.to_string(),
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
                let client = crate::relay_client_builder_with_proxy(s.config().use_proxy)
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .expect("c-two: failed to build reqwest Client for relay traffic");
                for seed_url in &s.config().seeds {
                    let join_url = peer_endpoint_url(seed_url, "/_peer/join");
                    let envelope = PeerEnvelope::new(
                        s.relay_id(),
                        PeerMessage::RelayJoin {
                            relay_id: s.relay_id().to_string(),
                            url: s.config().effective_advertise_url(),
                        },
                    );
                    if let Ok(resp) = client.post(&join_url).json(&envelope).send().await {
                        if let Ok(envelope) =
                            resp.json::<crate::relay::types::FullSyncEnvelope>().await
                        {
                            let Ok(snapshot) =
                                crate::relay::types::ValidatedFullSync::try_from(envelope)
                            else {
                                continue;
                            };
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

        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        );
        let sweeper = Self::idle_sweeper(state.clone(), idle_timeout_secs);

        tokio::select! {
            _ = server => {},
            _ = Self::command_loop(state.clone(), &mut cmd_rx) => {},
            _ = sweeper => {},
        }

        // Shutdown: broadcast leave, cancel background tasks
        let leave = PeerEnvelope::new(
            state.relay_id(),
            PeerMessage::RelayLeave {
                relay_id: state.relay_id().to_string(),
            },
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
                    tokio::spawn(async move { arc_client.close_shared().await });
                    if dead {
                        eprintln!("[relay] Evicted dead upstream: {name}");
                    } else {
                        eprintln!("[relay] Evicted idle upstream: {name}");
                    }
                }
            }
        }
    }

    async fn command_loop(state: Arc<RelayState>, cmd_rx: &mut mpsc::Receiver<Command>) {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                Command::RegisterUpstream {
                    name,
                    server_id,
                    address,
                    reply,
                } => {
                    eprintln!(
                        "[relay] Register command: name={name} server_id={server_id} address={address}"
                    );
                    if let Err(err) = validate_register_command(&state, &name, &server_id) {
                        eprintln!(
                            "[relay] Register command rejected: name={name} server_id={server_id} address={address} reason={err}"
                        );
                        let _ = reply.send(Err(err));
                        continue;
                    }
                    let replacement_candidate = match RouteAuthority::new(&state)
                        .prepare_candidate_registration(&name, &server_id, &address)
                        .await
                    {
                        Ok(replacement) => replacement,
                        Err(ControlError::AddressMismatch { .. })
                        | Err(ControlError::DuplicateRoute { .. }) => {
                            eprintln!(
                                "[relay] Register command rejected: name={name} server_id={server_id} address={address} reason=duplicate"
                            );
                            let _ = reply.send(Err(RelayControlError::DuplicateRoute { name }));
                            continue;
                        }
                        Err(ControlError::InvalidName { reason })
                        | Err(ControlError::InvalidServerId { reason })
                        | Err(ControlError::InvalidServerInstanceId { reason })
                        | Err(ControlError::InvalidAddress { reason })
                        | Err(ControlError::ContractMismatch { reason }) => {
                            eprintln!(
                                "[relay] Register command rejected: name={name} server_id={server_id} address={address} reason={reason}"
                            );
                            let _ = reply.send(Err(RelayControlError::Other(reason)));
                            continue;
                        }
                        Err(ControlError::OwnerMismatch) | Err(ControlError::NotFound) => {
                            eprintln!(
                                "[relay] Register command rejected: name={name} server_id={server_id} address={address} reason=owner_not_replaceable"
                            );
                            let _ = reply.send(Err(RelayControlError::DuplicateRoute { name }));
                            continue;
                        }
                    };
                    let result = {
                        let mut client =
                            IpcClient::with_config(&address, ClientIpcConfig::default());
                        match client.connect().await {
                            Ok(()) => {
                                let server_identity_matches =
                                    client.server_id() == Some(server_id.as_str());
                                if !server_identity_matches {
                                    close_client(client);
                                    eprintln!(
                                        "[relay] Register command rejected: name={name} server_id={server_id} address={address} reason=identity_mismatch"
                                    );
                                    let _ = reply.send(Err(RelayControlError::Other(format!(
                                        "IPC server identity mismatch for upstream '{name}'"
                                    ))));
                                    continue;
                                }
                                let Some(server_instance_id) =
                                    client.server_instance_id().map(ToOwned::to_owned)
                                else {
                                    close_client(client);
                                    eprintln!(
                                        "[relay] Register command rejected: name={name} server_id={server_id} address={address} reason=missing_server_instance_id"
                                    );
                                    let _ = reply.send(Err(RelayControlError::Other(format!(
                                        "IPC server identity mismatch for upstream '{name}'"
                                    ))));
                                    continue;
                                };
                                if let Err(ControlError::InvalidServerInstanceId { reason }) =
                                    RouteAuthority::new(&state)
                                        .validate_server_instance_id(&server_instance_id)
                                {
                                    close_client(client);
                                    eprintln!(
                                        "[relay] Register command rejected: name={name} server_id={server_id} address={address} reason={reason}"
                                    );
                                    let _ = reply.send(Err(RelayControlError::Other(reason)));
                                    continue;
                                }
                                if !client.has_route(&name) {
                                    close_client(client);
                                    eprintln!(
                                        "[relay] Register command rejected: name={name} server_id={server_id} address={address} reason=route_not_exported"
                                    );
                                    let _ = reply.send(Err(RelayControlError::Other(format!(
                                        "IPC upstream at {address} does not export route '{name}'"
                                    ))));
                                    continue;
                                };
                                let contract =
                                    match crate::relay::authority::read_ipc_route_contract(
                                        &client, &name,
                                    ) {
                                        Ok(contract) => contract,
                                        Err(ControlError::ContractMismatch { reason }) => {
                                            close_client(client);
                                            eprintln!(
                                                "[relay] Register command rejected: name={name} server_id={server_id} address={address} reason={reason}"
                                            );
                                            let _ =
                                                reply.send(Err(RelayControlError::Other(reason)));
                                            continue;
                                        }
                                        Err(ControlError::NotFound) => {
                                            close_client(client);
                                            eprintln!(
                                                "[relay] Register command rejected: name={name} server_id={server_id} address={address} reason=route_not_exported"
                                            );
                                            let _ = reply.send(Err(RelayControlError::Other(
                                                format!(
                                                    "IPC upstream at {address} does not export route '{name}'"
                                                ),
                                            )));
                                            continue;
                                        }
                                        Err(_) => unreachable!(
                                            "route contract attestation returns only contract errors"
                                        ),
                                    };
                                let client = Arc::new(client);
                                let replacement = match RouteAuthority::new(&state)
                                    .confirm_replacement_for_commit(replacement_candidate)
                                    .await
                                {
                                    Ok(replacement) => replacement,
                                    Err(ControlError::AddressMismatch { .. })
                                    | Err(ControlError::DuplicateRoute { .. })
                                    | Err(ControlError::OwnerMismatch)
                                    | Err(ControlError::NotFound) => {
                                        let close_client = client.clone();
                                        tokio::spawn(
                                            async move { close_client.close_shared().await },
                                        );
                                        eprintln!(
                                            "[relay] Register command rejected: name={name} server_id={server_id} address={address} reason=duplicate"
                                        );
                                        let _ = reply
                                            .send(Err(RelayControlError::DuplicateRoute { name }));
                                        continue;
                                    }
                                    Err(ControlError::InvalidName { reason })
                                    | Err(ControlError::InvalidServerId { reason })
                                    | Err(ControlError::InvalidServerInstanceId { reason })
                                    | Err(ControlError::InvalidAddress { reason })
                                    | Err(ControlError::ContractMismatch { reason }) => {
                                        let close_client = client.clone();
                                        tokio::spawn(
                                            async move { close_client.close_shared().await },
                                        );
                                        eprintln!(
                                            "[relay] Register command rejected: name={name} server_id={server_id} address={address} reason={reason}"
                                        );
                                        let _ = reply.send(Err(RelayControlError::Other(reason)));
                                        continue;
                                    }
                                };
                                match state.commit_register_upstream(
                                    name.clone(),
                                    server_id,
                                    server_instance_id,
                                    address,
                                    contract.crm_ns,
                                    contract.crm_name,
                                    contract.crm_ver,
                                    contract.abi_hash,
                                    contract.signature_hash,
                                    contract.max_payload_size,
                                    replacement,
                                ) {
                                    RegisterCommitResult::Registered { entry } => {
                                        tokio::spawn(async move { client.close_shared().await });
                                        eprintln!(
                                            "[relay] Register command committed: name={} server_id={} server_instance_id={} address={} crm={}/{}/{}",
                                            entry.name,
                                            entry.server_id.as_deref().unwrap_or(""),
                                            entry.server_instance_id.as_deref().unwrap_or(""),
                                            entry.ipc_address.as_deref().unwrap_or(""),
                                            entry.crm_ns,
                                            entry.crm_name,
                                            entry.crm_ver
                                        );
                                        broadcast_route_announce(&state, &entry);
                                        Ok(())
                                    }
                                    RegisterCommitResult::SameOwner { entry } => {
                                        tokio::spawn(async move { client.close_shared().await });
                                        eprintln!(
                                            "[relay] Register command same-owner: name={} server_id={} server_instance_id={} address={} crm={}/{}/{}",
                                            entry.name,
                                            entry.server_id.as_deref().unwrap_or(""),
                                            entry.server_instance_id.as_deref().unwrap_or(""),
                                            entry.ipc_address.as_deref().unwrap_or(""),
                                            entry.crm_ns,
                                            entry.crm_name,
                                            entry.crm_ver
                                        );
                                        Ok(())
                                    }
                                    RegisterCommitResult::Duplicate { .. }
                                    | RegisterCommitResult::ConflictingOwner { .. } => {
                                        tokio::spawn(async move { client.close_shared().await });
                                        eprintln!(
                                            "[relay] Register command rejected: name={name} reason=duplicate"
                                        );
                                        Err(RelayControlError::DuplicateRoute { name })
                                    }
                                    RegisterCommitResult::Invalid { reason } => {
                                        tokio::spawn(async move { client.close_shared().await });
                                        eprintln!(
                                            "[relay] Register command rejected: name={name} reason={reason}"
                                        );
                                        Err(RelayControlError::Other(reason))
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!(
                                    "[relay] Register command rejected: name={name} server_id={server_id} address={address} reason=connect_failed error={e}"
                                );
                                Err(RelayControlError::Other(format!("Failed to connect: {e}")))
                            }
                        }
                    };
                    let _ = reply.send(result);
                }
                Command::UnregisterUpstream {
                    name,
                    server_id,
                    reply,
                } => {
                    eprintln!("[relay] Unregister command: name={name} server_id={server_id}");
                    match state.unregister_upstream(&name, &server_id) {
                        UnregisterResult::Removed {
                            entry,
                            removed_at,
                            client,
                        } => {
                            if let Some(arc_client) = client {
                                tokio::spawn(async move { arc_client.close_shared().await });
                            }
                            eprintln!(
                                "[relay] Unregister command removed: name={} server_id={} removed_at={removed_at}",
                                entry.name,
                                entry.server_id.as_deref().unwrap_or("")
                            );
                            broadcast_route_withdraw(&state, &entry, removed_at);
                            let _ = reply.send(Ok(()));
                        }
                        UnregisterResult::AlreadyRemoved => {
                            eprintln!(
                                "[relay] Unregister command already removed: name={name} server_id={server_id}"
                            );
                            let _ = reply.send(Ok(()));
                        }
                        UnregisterResult::OwnerMismatch => {
                            eprintln!(
                                "[relay] Unregister command rejected: name={name} server_id={server_id} reason=owner_mismatch"
                            );
                            let _ =
                                reply.send(Err(format!("Server id does not own route: '{name}'")));
                        }
                        UnregisterResult::NotFound => {
                            eprintln!(
                                "[relay] Unregister command rejected: name={name} server_id={server_id} reason=not_found"
                            );
                            let _ = reply.send(Err(format!("Route name not registered: '{name}'")));
                        }
                    }
                }
                Command::ListRoutes { reply } => {
                    let routes: Vec<(String, String)> = state
                        .list_routes()
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{Command, RelayControlError, RelayServer};
    use c2_config::RelayConfig;
    use tokio::sync::{mpsc, oneshot};

    use crate::relay::disseminator::Disseminator;
    use crate::relay::peer::{PeerEnvelope, PeerMessage};
    use crate::relay::state::{RegisterCommitResult, RelayState};
    use crate::relay::test_support::{
        TEST_ABI_HASH, TEST_SIGNATURE_HASH, shutdown_live_server,
        start_live_server_with_identity_and_contracts, start_live_server_with_routes,
    };
    use crate::relay::types::PeerSnapshot;

    static NEXT_IPC_SUFFIX: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct RecordingDisseminator {
        envelopes: Mutex<Vec<PeerEnvelope>>,
    }

    fn source_between<'a>(source: &'a str, start: &str, end: &str) -> Option<&'a str> {
        let start_idx = source.find(start)?;
        let after_start = &source[start_idx..];
        let end_idx = after_start.find(end)?;
        Some(&after_start[..end_idx])
    }

    #[test]
    fn command_register_final_confirmation_stays_after_candidate_attestation() {
        let source = include_str!("server.rs");
        let body = source_between(
            source,
            "Command::RegisterUpstream {\n                    name,",
            "Command::UnregisterUpstream",
        )
        .expect("RegisterUpstream command branch should be found");
        let prepare = body
            .find(".prepare_candidate_registration(")
            .expect("command registration must prepare replacement eligibility");
        let connect = body
            .find("client.connect().await")
            .expect("command registration must connect candidate IPC");
        let read = body
            .find("read_ipc_route_contract")
            .expect("command registration must read candidate route contract");
        let confirm = body
            .find(".confirm_replacement_for_commit(")
            .expect("command registration must perform final replacement confirmation");
        let commit = body
            .find(".commit_register_upstream(")
            .expect("command registration must commit through RelayState");

        assert!(
            prepare < connect && connect < read,
            "preliminary eligibility must run before candidate IPC and route attestation"
        );
        assert!(
            read < confirm,
            "final replacement proof must be created after candidate route attestation"
        );
        assert!(
            confirm < commit,
            "relay commit must consume only post-attestation final proof"
        );
    }

    #[test]
    fn command_control_plane_logs_register_and_unregister_events() {
        let source = include_str!("server.rs");
        let register_body = source_between(
            source,
            "Command::RegisterUpstream {\n                    name,",
            "Command::UnregisterUpstream",
        )
        .expect("RegisterUpstream command branch should be found");
        let unregister_body = source_between(
            source,
            "Command::UnregisterUpstream {\n                    name,",
            "Command::ListRoutes",
        )
        .expect("UnregisterUpstream command branch should be found");

        for expected in [
            "[relay] Register command:",
            "[relay] Register command committed:",
            "[relay] Register command same-owner:",
            "[relay] Register command rejected:",
        ] {
            assert!(
                register_body.contains(expected),
                "command register path must log diagnostic event: {expected}"
            );
        }
        for expected in [
            "[relay] Unregister command:",
            "[relay] Unregister command removed:",
            "[relay] Unregister command already removed:",
            "[relay] Unregister command rejected:",
        ] {
            assert!(
                unregister_body.contains(expected),
                "command unregister path must log diagnostic event: {expected}"
            );
        }
    }

    impl RecordingDisseminator {
        fn envelopes(&self) -> Vec<PeerEnvelope> {
            self.envelopes.lock().unwrap().clone()
        }
    }

    impl Disseminator for RecordingDisseminator {
        fn broadcast(
            &self,
            envelope: PeerEnvelope,
            _peers: &[PeerSnapshot],
        ) -> Option<tokio::task::JoinHandle<()>> {
            self.envelopes.lock().unwrap().push(envelope);
            None
        }
    }

    fn command_loop_state_with_config(
        config: RelayConfig,
    ) -> (
        Arc<RelayState>,
        Arc<RecordingDisseminator>,
        mpsc::Sender<Command>,
        tokio::task::JoinHandle<()>,
    ) {
        let disseminator = Arc::new(RecordingDisseminator::default());
        let state = Arc::new(RelayState::new(Arc::new(config), disseminator.clone()));
        let (tx, mut rx) = mpsc::channel(8);
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            RelayServer::command_loop(task_state, &mut rx).await;
        });
        (state, disseminator, tx, task)
    }

    fn command_loop_state() -> (
        Arc<RelayState>,
        Arc<RecordingDisseminator>,
        mpsc::Sender<Command>,
        tokio::task::JoinHandle<()>,
    ) {
        command_loop_state_with_config(RelayConfig {
            relay_id: "relay-a".into(),
            advertise_url: "http://relay-a:8080".into(),
            ..RelayConfig::default()
        })
    }

    fn unique_ipc_address(label: &str) -> String {
        let suffix = NEXT_IPC_SUFFIX.fetch_add(1, Ordering::Relaxed);
        format!(
            "ipc://relay_server_{label}_{}_{}",
            std::process::id(),
            suffix
        )
    }

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

    #[test]
    fn relay_start_rejects_idle_timeout_millisecond_overflow() {
        let config = RelayConfig {
            bind: "127.0.0.1:0".to_string(),
            idle_timeout_secs: u64::MAX,
            ..RelayConfig::default()
        };

        let err = match RelayServer::start(config) {
            Ok(mut server) => {
                let _ = server.stop();
                panic!("invalid relay config should fail");
            }
            Err(err) => err,
        };

        assert!(err.contains("Invalid relay config"));
        assert!(err.contains("idle_timeout_secs"));
        assert!(err.contains("milliseconds"));
    }

    #[test]
    fn start_rejects_invalid_advertise_url_before_route_state() {
        let mut config = RelayConfig {
            bind: "127.0.0.1:0".into(),
            relay_id: "relay-invalid-url".into(),
            advertise_url: "not a url".into(),
            ..RelayConfig::default()
        };

        let err = match RelayServer::start(config.clone()) {
            Err(err) => err,
            Ok(mut relay) => {
                let _ = relay.stop();
                panic!("invalid advertise URL must fail");
            }
        };
        assert!(err.contains("advertise_url"));

        config.advertise_url = "ftp://relay-a:8080".into();
        let err = match RelayServer::start(config) {
            Err(err) => err,
            Ok(mut relay) => {
                let _ = relay.stop();
                panic!("non-http advertise URL must fail");
            }
        };
        assert!(err.contains("advertise_url"));
    }

    #[test]
    fn relay_control_unregister_rejects_wrong_server_id() {
        let address = unique_ipc_address("control_unregister_wrong_owner");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let server = rt.block_on(start_live_server_with_routes(
            &address,
            "server-grid",
            &["grid"],
        ));
        let config = RelayConfig {
            bind: "127.0.0.1:0".to_string(),
            ..RelayConfig::default()
        };
        let relay = RelayServer::start(config).expect("relay starts");

        relay
            .register_upstream("grid", "server-grid", &address)
            .expect("relay registers attested IPC upstream");

        assert!(relay.unregister_upstream("grid", "server-other").is_err());
        assert!(relay.unregister_upstream("grid", "server-grid").is_ok());
        rt.block_on(shutdown_live_server(&server));
    }

    #[tokio::test]
    async fn command_register_broadcasts_route_announce() {
        let (_state, disseminator, tx, task) = command_loop_state();
        let address = unique_ipc_address("command_broadcast");
        let server = start_live_server_with_routes(&address, "server-grid", &["grid"]).await;
        let (reply, result) = oneshot::channel();

        tx.send(Command::RegisterUpstream {
            name: "grid".into(),
            server_id: "server-grid".into(),
            address: address.clone(),
            reply,
        })
        .await
        .unwrap();

        result.await.unwrap().unwrap();
        drop(tx);
        task.await.unwrap();

        let envelopes = disseminator.envelopes();
        assert_eq!(envelopes.len(), 1);
        match &envelopes[0].message {
            PeerMessage::RouteAnnounce {
                name,
                relay_id,
                relay_url,
                ..
            } => {
                assert_eq!(name, "grid");
                assert_eq!(relay_id, "relay-a");
                assert_eq!(relay_url, "http://relay-a:8080");
            }
            other => panic!("expected RouteAnnounce, got {other:?}"),
        }
        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn command_register_broadcasts_attested_crm_tag() {
        let (_state, disseminator, tx, task) = command_loop_state();
        let address = unique_ipc_address("command_attested_contract");
        let server = start_live_server_with_routes(&address, "server-grid", &["grid"]).await;
        let (reply, result) = oneshot::channel();

        tx.send(Command::RegisterUpstream {
            name: "grid".into(),
            server_id: "server-grid".into(),
            address: address.clone(),
            reply,
        })
        .await
        .unwrap();

        result.await.unwrap().unwrap();
        drop(tx);
        task.await.unwrap();

        let envelopes = disseminator.envelopes();
        assert_eq!(envelopes.len(), 1);
        match &envelopes[0].message {
            PeerMessage::RouteAnnounce {
                crm_ns,
                crm_name,
                crm_ver,
                ..
            } => {
                assert_eq!(crm_ns, "test.echo");
                assert_eq!(crm_name, "Echo");
                assert_eq!(crm_ver, "0.1.0");
            }
            other => panic!("expected RouteAnnounce, got {other:?}"),
        }
        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn command_register_rejects_upstream_that_does_not_export_route() {
        let (state, disseminator, tx, task) = command_loop_state_with_config(RelayConfig {
            relay_id: "relay-a".into(),
            advertise_url: "http://relay-a:8080".into(),
            ..RelayConfig::default()
        });
        let address = unique_ipc_address("missing_route");
        let server = start_live_server_with_routes(&address, "server-grid", &["counter"]).await;
        let (reply, result) = oneshot::channel();

        tx.send(Command::RegisterUpstream {
            name: "grid".into(),
            server_id: "server-grid".into(),
            address: address.clone(),
            reply,
        })
        .await
        .unwrap();

        let err = result.await.unwrap().unwrap_err();
        assert!(matches!(
            err,
            RelayControlError::Other(message)
                if message == format!("IPC upstream at {address} does not export route 'grid'")
        ));
        assert!(
            state.resolve("grid").is_empty(),
            "relay command path must not advertise an unexported upstream route"
        );
        assert!(
            disseminator.envelopes().is_empty(),
            "rejected command registrations must not broadcast route announce"
        );

        drop(tx);
        task.await.unwrap();
        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn command_register_derives_crm_contract_from_ipc_handshake() {
        let (state, _disseminator, tx, task) = command_loop_state_with_config(RelayConfig {
            relay_id: "relay-a".into(),
            advertise_url: "http://relay-a:8080".into(),
            ..RelayConfig::default()
        });
        let address = unique_ipc_address("contract_attested");
        let server = start_live_server_with_routes(&address, "server-grid", &["grid"]).await;
        let (reply, result) = oneshot::channel();

        tx.send(Command::RegisterUpstream {
            name: "grid".into(),
            server_id: "server-grid".into(),
            address: address.clone(),
            reply,
        })
        .await
        .unwrap();

        result.await.unwrap().unwrap();
        let routes = state.resolve("grid");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].crm_ns, "test.echo");
        assert_eq!(routes[0].crm_ver, "0.1.0");

        drop(tx);
        task.await.unwrap();
        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn command_register_same_owner_rejects_contract_change() {
        let (state, disseminator, tx, task) = command_loop_state_with_config(RelayConfig {
            relay_id: "relay-a".into(),
            advertise_url: "http://relay-a:8080".into(),
            ..RelayConfig::default()
        });
        let address = unique_ipc_address("same_owner_contract_mismatch");
        match state.commit_register_upstream(
            "grid".into(),
            "server-grid".into(),
            "server-grid-instance".into(),
            address.clone(),
            "test.echo".into(),
            "Echo".into(),
            "0.1.0".into(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            None,
        ) {
            RegisterCommitResult::Registered { .. } => {}
            _ => panic!("unexpected initial registration result"),
        }
        let server = start_live_server_with_identity_and_contracts(
            &address,
            "server-grid",
            "server-grid-instance",
            &[(
                "grid",
                "test.other",
                "OtherGrid",
                "0.1.0",
                TEST_ABI_HASH,
                TEST_SIGNATURE_HASH,
            )],
        )
        .await;
        let (reply, result) = oneshot::channel();

        tx.send(Command::RegisterUpstream {
            name: "grid".into(),
            server_id: "server-grid".into(),
            address: address.clone(),
            reply,
        })
        .await
        .unwrap();

        let err = result.await.unwrap().unwrap_err();
        assert!(matches!(
            err,
            RelayControlError::Other(message)
                if message.contains("CRM contract mismatch")
        ));
        let routes = state.resolve("grid");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].crm_ns, "test.echo");
        assert_eq!(routes[0].crm_name, "Echo");
        assert!(
            disseminator.envelopes().is_empty(),
            "rejected same-owner contract changes must not broadcast route announce"
        );

        drop(tx);
        task.await.unwrap();
        shutdown_live_server(&server).await;
    }

    #[tokio::test]
    async fn command_register_duplicate_live_owner_rejects_before_candidate_connect() {
        let (state, disseminator, tx, task) = command_loop_state_with_config(RelayConfig {
            relay_id: "relay-a".into(),
            advertise_url: "http://relay-a:8080".into(),
            ..RelayConfig::default()
        });
        let old_address = unique_ipc_address("duplicate_live_old");
        let old_server = start_live_server_with_routes(&old_address, "server-old", &["grid"]).await;
        let (first_reply, first_result) = oneshot::channel();

        tx.send(Command::RegisterUpstream {
            name: "grid".into(),
            server_id: "server-old".into(),
            address: old_address.clone(),
            reply: first_reply,
        })
        .await
        .unwrap();
        first_result.await.unwrap().unwrap();

        let bad_candidate_address = unique_ipc_address("duplicate_live_bad_candidate");
        let (second_reply, second_result) = oneshot::channel();
        tx.send(Command::RegisterUpstream {
            name: "grid".into(),
            server_id: "server-new".into(),
            address: bad_candidate_address,
            reply: second_reply,
        })
        .await
        .unwrap();

        let err = second_result.await.unwrap().unwrap_err();
        assert!(matches!(
            err,
            RelayControlError::DuplicateRoute { name } if name == "grid"
        ));
        let routes = state.resolve("grid");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].server_id.as_deref(), Some("server-old"));
        assert_eq!(routes[0].ipc_address.as_deref(), Some(old_address.as_str()));
        assert_eq!(
            disseminator.envelopes().len(),
            1,
            "duplicate command registrations must not broadcast route announce"
        );

        drop(tx);
        task.await.unwrap();
        shutdown_live_server(&old_server).await;
    }

    #[tokio::test]
    async fn command_register_replaces_dead_evicted_owner_after_candidate_attestation() {
        let (state, disseminator, tx, task) = command_loop_state_with_config(RelayConfig {
            relay_id: "relay-a".into(),
            advertise_url: "http://relay-a:8080".into(),
            ..RelayConfig::default()
        });
        let old_address = unique_ipc_address("dead_evicted_old");
        let old_server = start_live_server_with_routes(&old_address, "server-old", &["grid"]).await;
        let (first_reply, first_result) = oneshot::channel();

        tx.send(Command::RegisterUpstream {
            name: "grid".into(),
            server_id: "server-old".into(),
            address: old_address,
            reply: first_reply,
        })
        .await
        .unwrap();
        first_result.await.unwrap().unwrap();
        if let Some(client) = state.evict_connection("grid") {
            client.close_shared().await;
        }
        shutdown_live_server(&old_server).await;

        let new_address = unique_ipc_address("dead_evicted_new");
        let new_server = start_live_server_with_routes(&new_address, "server-new", &["grid"]).await;
        let (second_reply, second_result) = oneshot::channel();
        tx.send(Command::RegisterUpstream {
            name: "grid".into(),
            server_id: "server-new".into(),
            address: new_address.clone(),
            reply: second_reply,
        })
        .await
        .unwrap();

        second_result.await.unwrap().unwrap();
        let routes = state.resolve("grid");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].server_id.as_deref(), Some("server-new"));
        assert_eq!(routes[0].ipc_address.as_deref(), Some(new_address.as_str()));
        assert_eq!(disseminator.envelopes().len(), 2);

        drop(tx);
        task.await.unwrap();
        shutdown_live_server(&new_server).await;
    }

    #[tokio::test]
    async fn command_unregister_broadcasts_route_withdraw() {
        let (_state, disseminator, tx, task) = command_loop_state();
        let address = unique_ipc_address("command_unregister_broadcast");
        let server = start_live_server_with_routes(&address, "server-grid", &["grid"]).await;
        let (register_reply, register_result) = oneshot::channel();
        tx.send(Command::RegisterUpstream {
            name: "grid".into(),
            server_id: "server-grid".into(),
            address: address.clone(),
            reply: register_reply,
        })
        .await
        .unwrap();
        register_result.await.unwrap().unwrap();

        let (unregister_reply, unregister_result) = oneshot::channel();
        tx.send(Command::UnregisterUpstream {
            name: "grid".into(),
            server_id: "server-grid".into(),
            reply: unregister_reply,
        })
        .await
        .unwrap();

        unregister_result.await.unwrap().unwrap();
        drop(tx);
        task.await.unwrap();

        let envelopes = disseminator.envelopes();
        assert_eq!(envelopes.len(), 2);
        match &envelopes[1].message {
            PeerMessage::RouteWithdraw { name, relay_id, .. } => {
                assert_eq!(name, "grid");
                assert_eq!(relay_id, "relay-a");
            }
            other => panic!("expected RouteWithdraw, got {other:?}"),
        }
        shutdown_live_server(&server).await;
    }
}
