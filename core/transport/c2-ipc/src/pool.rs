//! Reference-counted pool of [`SyncClient`] instances.
//!
//! Clients connecting to the same server address share a single
//! `SyncClient`. When all references are released the client is
//! kept alive for a grace period before being destroyed.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use c2_mem::{MemPool, PoolConfig};

/// Label counter for client pools. MemPool adds its incarnation and owns
/// platform segment-name derivation.
static CLIENT_POOL_GEN: AtomicU64 = AtomicU64::new(0);
const CONNECT_TRANSIENT_RETRY_ATTEMPTS: usize = 3;

use crate::client::{ClientIpcConfig, IpcError};
use crate::sync_client::SyncClient;

pub(crate) fn pool_config_from_client_config(cfg: &ClientIpcConfig) -> PoolConfig {
    PoolConfig {
        segment_size: cfg.pool_segment_size as usize,
        max_segments: cfg.max_pool_segments as usize,
        ..PoolConfig::default()
    }
}

fn is_transient_connect_error(error: &IpcError) -> bool {
    matches!(
        error,
        IpcError::Io(io_error)
            if matches!(
                io_error.kind(),
                ErrorKind::UnexpectedEof
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::NotConnected
            )
    )
}

fn connect_with_transient_retry(
    address: &str,
    cfg: &ClientIpcConfig,
) -> Result<SyncClient, IpcError> {
    let pool_config = pool_config_from_client_config(cfg);

    for attempt in 0..CONNECT_TRANSIENT_RETRY_ATTEMPTS {
        let counter = CLIENT_POOL_GEN.fetch_add(1, Ordering::Relaxed) as u32;
        let prefix = format!("/cc3c{:08x}{:08x}", std::process::id(), counter);
        let pool = Arc::new(Mutex::new(MemPool::new_with_prefix(
            pool_config.clone(),
            prefix,
        )));

        match SyncClient::connect(address, Some(pool), cfg.clone()) {
            Ok(client) => return Ok(client),
            Err(error)
                if attempt + 1 < CONNECT_TRANSIENT_RETRY_ATTEMPTS
                    && is_transient_connect_error(&error) =>
            {
                std::thread::sleep(Duration::from_millis(10 * (attempt as u64 + 1)));
            }
            Err(error) => return Err(error),
        }
    }

    unreachable!("connect retry loop always returns before exhausting attempts")
}

// ── Pool entry ───────────────────────────────────────────────────────────

struct PoolEntry {
    client: Arc<SyncClient>,
    ref_count: usize,
    /// Set to `Some(Instant::now())` when `ref_count` drops to 0.
    last_release: Option<Instant>,
}

// ── ClientPool ───────────────────────────────────────────────────────────

/// Reference-counted pool of `SyncClient` instances.
///
/// Clients connecting to the same server address share a single
/// `SyncClient`. When all references are released, the client is
/// kept alive for a grace period before being destroyed.
pub struct ClientPool {
    entries: Mutex<HashMap<String, PoolEntry>>,
    grace_period: Duration,
    default_config: Mutex<Option<ClientIpcConfig>>,
}

// Compile-time assertion: ClientPool must be Send + Sync.
const _: () = {
    fn _assert_send<T: Send>() {}
    fn _assert_sync<T: Sync>() {}
    fn _assertions() {
        _assert_send::<ClientPool>();
        _assert_sync::<ClientPool>();
    }
};

impl ClientPool {
    /// Create a new pool with the given grace period.
    pub fn new(grace_period: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            grace_period,
            default_config: Mutex::new(None),
        }
    }

    /// Set the default IPC config for newly created clients.
    pub fn set_default_config(&self, config: ClientIpcConfig) {
        *self.default_config.lock() = Some(config);
    }

    /// Acquire a client for `address`. Creates and connects if needed.
    /// Increments reference count.
    pub fn acquire(
        &self,
        address: &str,
        config: Option<&ClientIpcConfig>,
    ) -> Result<Arc<SyncClient>, IpcError> {
        // Sweep stale entries before potentially creating a new one.
        self.sweep_expired();

        let mut entries = self.entries.lock();

        // Fast path: existing connected client.
        if let Some(entry) = entries.get_mut(address) {
            if entry.client.is_connected() {
                entry.ref_count += 1;
                entry.last_release = None;
                return Ok(Arc::clone(&entry.client));
            }
            // Stale — remove and fall through to create a new one.
            entries.remove(address);
        }

        // Resolve config: explicit > default > ClientIpcConfig::default().
        let cfg = match config {
            Some(c) => c.clone(),
            None => self.default_config.lock().clone().unwrap_or_default(),
        };

        // Drop the entries lock before connecting (connect may block).
        drop(entries);

        let client = connect_with_transient_retry(address, &cfg)?;
        let client = Arc::new(client);

        let mut entries = self.entries.lock();

        // Another thread may have raced and inserted the same address.
        if let Some(entry) = entries.get_mut(address)
            && entry.client.is_connected()
        {
            entry.ref_count += 1;
            entry.last_release = None;
            return Ok(Arc::clone(&entry.client));
            // Stale racing entry — replace below.
        }

        entries.insert(
            address.to_owned(),
            PoolEntry {
                client: Arc::clone(&client),
                ref_count: 1,
                last_release: None,
            },
        );

        Ok(client)
    }

    /// Decrement reference count. When it reaches 0, mark for grace-period
    /// cleanup.
    pub fn release(&self, address: &str) {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.get_mut(address) {
            if entry.ref_count == 0 {
                eprintln!("ClientPool::release: ref_count already 0 for {address}");
                return;
            }
            entry.ref_count -= 1;
            if entry.ref_count == 0 {
                entry.last_release = Some(Instant::now());
            }
        } else {
            eprintln!("ClientPool::release: no entry for {address}");
        }
    }

    /// Release one reference only when the pool still contains the acquired client.
    ///
    /// This identity check prevents a late drop from an evicted connection
    /// decrementing the reference count of a replacement at the same address.
    pub fn release_if_same(&self, address: &str, observed: &Arc<SyncClient>) -> bool {
        let mut entries = self.entries.lock();
        let Some(entry) = entries.get_mut(address) else {
            return false;
        };
        if !Arc::ptr_eq(&entry.client, observed) {
            return false;
        }
        if entry.ref_count == 0 {
            return false;
        }
        entry.ref_count -= 1;
        if entry.ref_count == 0 {
            entry.last_release = Some(Instant::now());
        }
        true
    }

    /// Remove one observed unusable client without evicting a racing replacement.
    ///
    /// Callers must use this only after a pre-dispatch operation proves that
    /// the exact acquired connection can no longer serve requests.
    pub fn discard_if_same(&self, address: &str, observed: &Arc<SyncClient>) -> bool {
        let mut entries = self.entries.lock();
        let is_same = entries
            .get(address)
            .is_some_and(|entry| Arc::ptr_eq(&entry.client, observed));
        if is_same {
            entries.remove(address);
        }
        is_same
    }

    /// Sweep expired entries that have been unreferenced longer than
    /// `grace_period`. Call this periodically from SDK bindings or before
    /// acquire.
    pub fn sweep_expired(&self) {
        let mut entries = self.entries.lock();
        let grace = self.grace_period;
        entries.retain(|_addr, entry| {
            if entry.ref_count == 0
                && let Some(released_at) = entry.last_release
                && released_at.elapsed() >= grace
            {
                // Drop the Arc — connection closes when last ref is gone.
                return false;
            }
            true
        });
    }

    /// Destroy all clients immediately (for shutdown / testing).
    pub fn shutdown_all(&self) {
        let mut entries = self.entries.lock();
        entries.clear(); // Arcs are dropped → connections close.
    }

    /// Number of active entries (for testing).
    pub fn active_count(&self) -> usize {
        self.entries.lock().len()
    }

    /// Reference count for an address (for testing).
    pub fn refcount(&self, address: &str) -> usize {
        self.entries.lock().get(address).map_or(0, |e| e.ref_count)
    }

    /// Check if a client exists for the address (for testing).
    pub fn has_client(&self, address: &str) -> bool {
        self.entries.lock().contains_key(address)
    }
}

// ── Singleton ────────────────────────────────────────────────────────────

static GLOBAL_POOL: OnceLock<ClientPool> = OnceLock::new();

impl ClientPool {
    /// Return the process-level singleton.
    pub fn instance() -> &'static ClientPool {
        GLOBAL_POOL.get_or_init(|| ClientPool::new(Duration::from_secs(60)))
    }

    /// Reset the singleton (testing only).
    /// `OnceLock` cannot be truly reset, so this shuts down all entries.
    pub fn reset_instance() {
        if let Some(pool) = GLOBAL_POOL.get() {
            pool.shutdown_all();
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::IpcClient;
    use c2_local::{LocalEndpoint, LocalListener};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::thread;
    use tokio::io::AsyncReadExt;

    use c2_server::{
        ConcurrencyMode, CrmCallback, CrmError, RequestData, ResponseMeta, RouteBuildSpec,
        SchedulerLimits, Server, ServerIpcConfig,
    };

    struct Echo;

    impl CrmCallback for Echo {
        fn invoke(
            &self,
            _route_name: &str,
            _method_idx: u16,
            _request: RequestData,
            _response_pool: Arc<parking_lot::RwLock<c2_mem::MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            Ok(ResponseMeta::Inline(b"ok".to_vec()))
        }
    }

    fn unique_ipc_address(prefix: &str) -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        format!(
            "ipc://{}_{}_{}",
            prefix,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn expected_contract(name: &str) -> c2_contract::ExpectedRouteContract {
        c2_contract::ExpectedRouteContract {
            route_name: name.to_string(),
            crm_ns: "test.pool".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .to_string(),
        }
    }

    async fn register_test_route(server: &Server, name: &str) {
        let expected = expected_contract(name);
        let built = server
            .build_route(
                RouteBuildSpec {
                    name: name.to_string(),
                    crm_ns: expected.crm_ns,
                    crm_name: expected.crm_name,
                    crm_ver: expected.crm_ver,
                    abi_hash: expected.abi_hash,
                    signature_hash: expected.signature_hash,
                    method_names: vec!["ping".to_string()],
                    access_map: HashMap::new(),
                    concurrency_mode: ConcurrencyMode::ReadParallel,
                    limits: SchedulerLimits::default(),
                },
                Arc::new(Echo),
            )
            .expect("test route should build");
        let reservation = server
            .reserve_route(built)
            .await
            .expect("test route should reserve");
        server
            .commit_reserved_route(reservation)
            .await
            .expect("test route should commit");
    }

    #[test]
    fn test_pool_new() {
        let pool = ClientPool::new(Duration::from_secs(30));
        assert_eq!(pool.active_count(), 0);
    }

    #[test]
    fn test_pool_singleton() {
        let p1 = ClientPool::instance() as *const ClientPool;
        let p2 = ClientPool::instance() as *const ClientPool;
        assert_eq!(p1, p2, "singleton must return the same instance");
    }

    #[test]
    fn test_pool_grace_period_sweep() {
        let pool = ClientPool::new(Duration::from_millis(50));

        // Manually insert a fake entry with ref_count=0 and old release time.
        {
            let client = make_disconnected_client();
            let mut entries = pool.entries.lock();
            entries.insert(
                "ipc://fake".to_owned(),
                PoolEntry {
                    client: Arc::new(client),
                    ref_count: 0,
                    last_release: Some(Instant::now() - Duration::from_millis(200)),
                },
            );
        }
        assert_eq!(pool.active_count(), 1);

        pool.sweep_expired();
        assert_eq!(pool.active_count(), 0, "expired entry should be swept");
    }

    #[test]
    fn test_pool_grace_period_not_expired() {
        let pool = ClientPool::new(Duration::from_secs(60));

        {
            let client = make_disconnected_client();
            let mut entries = pool.entries.lock();
            entries.insert(
                "ipc://recent".to_owned(),
                PoolEntry {
                    client: Arc::new(client),
                    ref_count: 0,
                    last_release: Some(Instant::now()),
                },
            );
        }
        assert_eq!(pool.active_count(), 1);

        pool.sweep_expired();
        assert_eq!(
            pool.active_count(),
            1,
            "recently released entry should survive"
        );
    }

    #[test]
    fn test_pool_refcount_tracking() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let addr = "ipc://reftest";

        // Insert a fake connected-looking entry for refcount math.
        {
            let client = make_disconnected_client();
            let mut entries = pool.entries.lock();
            entries.insert(
                addr.to_owned(),
                PoolEntry {
                    client: Arc::new(client),
                    ref_count: 2,
                    last_release: None,
                },
            );
        }

        assert_eq!(pool.refcount(addr), 2);

        pool.release(addr);
        assert_eq!(pool.refcount(addr), 1);
        assert!(pool.has_client(addr));

        pool.release(addr);
        assert_eq!(pool.refcount(addr), 0);
        // last_release should now be set; entry still present.
        assert!(pool.has_client(addr));
    }

    #[test]
    fn test_pool_release_unknown_address() {
        // Should not panic — just print a warning.
        let pool = ClientPool::new(Duration::from_secs(30));
        pool.release("ipc://nonexistent");
    }

    #[test]
    fn test_pool_release_already_zero() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let addr = "ipc://zero";

        {
            let client = make_disconnected_client();
            let mut entries = pool.entries.lock();
            entries.insert(
                addr.to_owned(),
                PoolEntry {
                    client: Arc::new(client),
                    ref_count: 0,
                    last_release: Some(Instant::now()),
                },
            );
        }

        // Should not panic or underflow.
        pool.release(addr);
        assert_eq!(pool.refcount(addr), 0);
    }

    #[test]
    fn discard_if_same_removes_only_the_observed_stale_client() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let address = "ipc://stale";
        let observed = Arc::new(make_disconnected_client());
        let raced_replacement = Arc::new(make_disconnected_client());

        pool.entries.lock().insert(
            address.to_string(),
            PoolEntry {
                client: Arc::clone(&observed),
                ref_count: 1,
                last_release: None,
            },
        );

        assert!(!pool.discard_if_same(address, &raced_replacement));
        assert!(pool.has_client(address));
        assert!(pool.discard_if_same(address, &observed));
        assert!(!pool.has_client(address));
    }

    #[test]
    fn release_if_same_cannot_decrement_a_racing_replacement() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let address = "ipc://replacement";
        let observed = Arc::new(make_disconnected_client());
        let replacement = Arc::new(make_disconnected_client());

        pool.entries.lock().insert(
            address.to_string(),
            PoolEntry {
                client: Arc::clone(&replacement),
                ref_count: 1,
                last_release: None,
            },
        );

        assert!(!pool.release_if_same(address, &observed));
        assert_eq!(pool.refcount(address), 1);
        assert!(pool.release_if_same(address, &replacement));
        assert_eq!(pool.refcount(address), 0);
    }

    #[test]
    fn test_pool_shutdown_all() {
        let pool = ClientPool::new(Duration::from_secs(30));

        {
            let c1 = make_disconnected_client();
            let c2 = make_disconnected_client();
            let mut entries = pool.entries.lock();
            entries.insert(
                "ipc://a".to_owned(),
                PoolEntry {
                    client: Arc::new(c1),
                    ref_count: 1,
                    last_release: None,
                },
            );
            entries.insert(
                "ipc://b".to_owned(),
                PoolEntry {
                    client: Arc::new(c2),
                    ref_count: 0,
                    last_release: Some(Instant::now()),
                },
            );
        }
        assert_eq!(pool.active_count(), 2);

        pool.shutdown_all();
        assert_eq!(pool.active_count(), 0);
    }

    #[test]
    fn test_pool_acquire_no_server() {
        // acquire() should fail gracefully when no server is listening.
        let pool = ClientPool::new(Duration::from_secs(30));
        let result = pool.acquire("ipc:///nonexistent_socket_path", None);
        assert!(result.is_err(), "acquire without server should fail");
    }

    #[test]
    fn acquire_retries_transient_handshake_eof() {
        let address = format!("ipc://pool_retry_{}", std::process::id());
        let endpoint = LocalEndpoint::from_address(&address).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let server_thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let mut listener = LocalListener::bind(&endpoint).unwrap();
                ready_tx.send(()).unwrap();
                tokio::time::timeout(Duration::from_secs(10), async {
                    for attempt in 0..2 {
                        let mut stream = listener.accept().await.unwrap();

                        let mut len_buf = [0_u8; 4];
                        stream.read_exact(&mut len_buf).await.unwrap();
                        let body_len = u32::from_le_bytes(len_buf) as usize;
                        let mut body = vec![0_u8; body_len];
                        stream.read_exact(&mut body).await.unwrap();

                        if attempt == 0 {
                            continue;
                        }

                        let route = c2_wire::handshake::RouteInfo {
                            name: "grid".to_string(),
                            route_uid: "grid-route-uid-0001".to_string(),
                            route_revision: 1,
                            crm_ns: "test.pool".to_string(),
                            crm_name: "Grid".to_string(),
                            crm_ver: "0.1.0".to_string(),
                            abi_hash:
                                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                                    .to_string(),
                            signature_hash:
                                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                                    .to_string(),
                            max_payload_size: 1024,
                            methods: vec![c2_wire::handshake::MethodEntry {
                                name: "ping".to_string(),
                                index: 0,
                            }],
                        };
                        let identity = c2_wire::handshake::ServerIdentity {
                            server_id: "pool-retry-server".to_string(),
                            server_instance_id: "pool-retry-instance".to_string(),
                        };
                        let payload = c2_wire::handshake::encode_server_handshake(
                            &[],
                            c2_wire::handshake::CAP_CALL_V2
                                | c2_wire::handshake::CAP_METHOD_IDX
                                | c2_wire::handshake::CAP_CHUNKED,
                            &[route],
                            "",
                            &identity,
                        )
                        .unwrap();
                        let frame = c2_wire::frame::encode_frame(
                            0,
                            c2_wire::flags::FLAG_HANDSHAKE | c2_wire::flags::FLAG_RESPONSE,
                            &payload,
                        );
                        stream.write_all(&frame).await.unwrap();
                        let mut extra_len = [0_u8; 4];
                        match tokio::time::timeout(
                            Duration::from_millis(150),
                            stream.read_exact(&mut extra_len),
                        )
                        .await
                        {
                            Err(_) => {}
                            Ok(Ok(_)) => {
                                panic!("direct IPC connect must not open a route-watch stream")
                            }
                            Ok(Err(err)) => panic!("unexpected post-handshake read error: {err}"),
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                })
                .await
                .expect("retry handshake fixture must finish");
            });
        });
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("retry listener readiness");

        let pool = ClientPool::new(Duration::from_secs(60));
        let client = pool.acquire(&address, None).unwrap();
        assert_eq!(client.route_names(), vec!["grid".to_string()]);
        pool.release(&address);

        server_thread.join().unwrap();
    }

    #[test]
    fn pooled_direct_client_observes_route_registered_after_handshake() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let address = unique_ipc_address("pool_live_route_refresh");
            let server = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
            register_test_route(&server, "manager").await;
            let runner = {
                let server = Arc::clone(&server);
                tokio::spawn(async move { server.run().await })
            };
            server
                .wait_until_responsive(Duration::from_secs(2))
                .await
                .expect("server should be responsive");

            let pool = Arc::new(ClientPool::new(Duration::from_secs(60)));
            let acquire_pool = Arc::clone(&pool);
            let acquire_address = address.clone();
            let client =
                tokio::task::spawn_blocking(move || acquire_pool.acquire(&acquire_address, None))
                    .await
                    .expect("acquire task should complete")
                    .expect("manager client");
            assert!(client.route_names().contains(&"manager".to_string()));
            assert!(!client.route_names().contains(&"builder".to_string()));

            register_test_route(&server, "builder").await;
            let builder_contract = expected_contract("builder");

            let ensure_client = Arc::clone(&client);
            let ensure_contract = builder_contract.clone();
            let binding =
                tokio::task::spawn_blocking(move || ensure_client.acquire_route(&ensure_contract))
                    .await
                    .expect("ensure task should complete")
                    .expect("direct pooled IPC client should acquire builder through route lookup");
            assert_eq!(binding.route_name(), "builder");

            assert!(
                client.route_names().contains(&"builder".to_string()),
                "route lookup should cache the acquired builder route without watch"
            );
            pool.release(&address);

            server
                .shutdown_and_wait(Duration::from_secs(2))
                .await
                .expect("server should shut down");
            runner.await.unwrap().unwrap();
        });
    }

    #[test]
    fn registration_attestation_accepts_committed_closed_route_without_business_acquire() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let address = unique_ipc_address("registration_closed_route");
            let server = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
            let expected = expected_contract("grid");
            let built = server
                .build_route(
                    RouteBuildSpec {
                        name: "grid".to_string(),
                        crm_ns: expected.crm_ns.clone(),
                        crm_name: expected.crm_name.clone(),
                        crm_ver: expected.crm_ver.clone(),
                        abi_hash: expected.abi_hash.clone(),
                        signature_hash: expected.signature_hash.clone(),
                        method_names: vec!["ping".to_string()],
                        access_map: HashMap::new(),
                        concurrency_mode: ConcurrencyMode::ReadParallel,
                        limits: SchedulerLimits::default(),
                    },
                    Arc::new(Echo),
                )
                .expect("test route should build");
            let reservation = server.reserve_route(built).await.expect("reserve route");
            let admission = server
                .commit_reserved_route_closed(reservation)
                .await
                .expect("closed registration commit");
            let runner = {
                let server = Arc::clone(&server);
                tokio::spawn(async move { server.run().await })
            };
            server
                .wait_until_responsive(Duration::from_secs(2))
                .await
                .expect("server should be responsive");

            let mut client = IpcClient::with_config(&address, ClientIpcConfig::default());
            client.connect().await.expect("client connects");
            assert!(
                matches!(
                    client.acquire_route(&expected).await,
                    Err(IpcError::RouteClosed { .. })
                ),
                "business acquire must reject closed registration routes"
            );
            let binding = client
                .attest_route_for_registration(&expected)
                .await
                .expect("registration attestation accepts committed closed route");
            assert_eq!(binding.route_name(), "grid");

            server
                .open_route_admission(admission)
                .await
                .expect("route admission opens for cleanup");
            client.close().await;
            server
                .shutdown_and_wait(Duration::from_secs(2))
                .await
                .expect("server should shut down");
            runner.await.unwrap().unwrap();
        });
    }

    #[test]
    fn test_pool_set_default_config() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let cfg = ClientIpcConfig {
            base: c2_config::BaseIpcConfig {
                chunk_size: 65536,
                ..c2_config::BaseIpcConfig::default()
            },
            shm_threshold: 1024,
        };
        pool.set_default_config(cfg);
        // Verify the config is stored (indirectly — acquire would use it).
        let stored = pool.default_config.lock();
        assert!(stored.is_some());
        let c = stored.as_ref().unwrap();
        assert_eq!(c.shm_threshold, 1024);
        assert_eq!(c.chunk_size, 65536);
    }

    #[test]
    fn pool_config_from_client_config_uses_max_pool_segments() {
        let cfg = ClientIpcConfig {
            base: c2_config::BaseIpcConfig {
                pool_segment_size: 65_536,
                max_pool_segments: 3,
                ..c2_config::BaseIpcConfig::default()
            },
            shm_threshold: 1024,
        };

        let pc = pool_config_from_client_config(&cfg);

        assert_eq!(pc.segment_size, 65_536);
        assert_eq!(pc.max_segments, 3);
    }

    // ── Helper ───────────────────────────────────────────────────────────

    #[test]
    fn test_client_pool_unique_prefixes() {
        // Verify that successive client MemPool creations get different
        // SHM prefixes via CLIENT_POOL_GEN counter.
        let pc = PoolConfig::default();
        let c1 = CLIENT_POOL_GEN.fetch_add(1, Ordering::Relaxed) as u32;
        let p1 = format!("/cc3c{:08x}{:08x}", std::process::id(), c1);
        let c2 = CLIENT_POOL_GEN.fetch_add(1, Ordering::Relaxed) as u32;
        let p2 = format!("/cc3c{:08x}{:08x}", std::process::id(), c2);
        assert_ne!(p1, p2, "consecutive prefixes must differ");
        // Verify the pools can be created with these prefixes.
        let pool1 = MemPool::new_with_prefix(pc.clone(), p1.clone());
        let pool2 = MemPool::new_with_prefix(pc.clone(), p2.clone());
        let repeated_label = MemPool::new_with_prefix(pc, p1.clone());
        assert!(pool1.prefix().starts_with(&format!("{p1}_")));
        assert!(pool2.prefix().starts_with(&format!("{p2}_")));
        assert_ne!(pool1.prefix(), repeated_label.prefix());
        assert_ne!(pool1.prefix(), pool2.prefix());
        assert!(pool1.prefix().len() <= c2_contract::MAX_WIRE_TEXT_BYTES);
    }

    /// Create a disconnected SyncClient for testing pool bookkeeping.
    fn make_disconnected_client() -> SyncClient {
        SyncClient::new_unconnected("ipc:///pool_test_fake")
    }
}
