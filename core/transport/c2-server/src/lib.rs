mod admission;
mod catalog;
mod chunk_ordering;
pub mod config;
pub mod connection;
mod dispatcher;
pub mod heartbeat;
pub mod response;
pub mod runtime;
mod scheduler;
pub mod server;

pub use c2_local::{
    ConnectionClosed, ConnectionEndReason, ConnectionEvent, ConnectionEventSource,
    ConnectionEvents, ConnectionEventsError, ConnectionObservation,
};
pub use config::ServerIpcConfig;
pub use connection::Connection;
pub use dispatcher::{
    BuiltRoute, CrmCallback, CrmError, PeerShmRequest, PreparedShmResponse, RequestData,
    RequestLease, ResponseMeta, RouteBuildSpec,
};
pub use heartbeat::{HeartbeatResult, run_heartbeat};
pub use runtime::{ServerRuntimeBuilder, ServerRuntimeOptions};
pub use scheduler::{
    AccessLevel, ConcurrencyMode, RouteConcurrencyHandle, SchedulerAcquireError, SchedulerGuard,
    SchedulerLimits, SchedulerSnapshot,
};
pub use server::{
    DIRECT_IPC_SHUTDOWN_REASON, Server, ServerError, ServerFailure, ServerIdentity,
    ServerLifecycleState, ServerMemorySnapshot, ServerRouteCloseOutcome, ServerShutdownCompletion,
    ServerStartupStage, ServerStartupTimeout,
};

pub use response::ResponsePool;

#[cfg(test)]
mod test_support {
    use c2_local::{DEFAULT_CONNECT_TIMEOUT, EndpointReapResult, LocalListener, LocalStream};

    pub(crate) async fn isolated_pair() -> (LocalStream, LocalStream) {
        #[cfg(unix)]
        let root = tempfile::Builder::new()
            .prefix("r07")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(unix)]
        let context = c2_config::LocalEndpointContext::with_unix_root(root.path()).unwrap();
        #[cfg(windows)]
        let context = c2_config::LocalEndpointContext::default_for_platform().unwrap();
        let endpoint = context
            .endpoint(&format!("ipc://r07-{}", uuid::Uuid::new_v4().simple()))
            .unwrap();
        let mut listener = LocalListener::bind(&endpoint).unwrap();
        let pair = tokio::try_join!(
            LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
            listener.accept()
        )
        .unwrap();
        assert!(matches!(
            listener.close(),
            EndpointReapResult::Reaped | EndpointReapResult::NotApplicable
        ));
        #[cfg(unix)]
        assert!(
            std::fs::read_dir(root.path())
                .unwrap()
                .all(|entry| matches!(
                    entry.unwrap().file_name().to_str(),
                    Some(".gate" | ".gate.marker")
                )),
            "task root must have no socket or lease after listener close"
        );
        pair
    }
}
