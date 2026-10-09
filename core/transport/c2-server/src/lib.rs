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

pub use config::ServerIpcConfig;
pub use connection::Connection;
pub use dispatcher::{
    BuiltRoute, CrmCallback, CrmError, RequestData, RequestLease, ResponseMeta, RouteBuildSpec,
};
pub use heartbeat::{HeartbeatResult, run_heartbeat};
pub use runtime::{ServerRuntimeBuilder, ServerRuntimeOptions};
pub use scheduler::{
    AccessLevel, ConcurrencyMode, RouteConcurrencyHandle, SchedulerAcquireError, SchedulerGuard,
    SchedulerLimits, SchedulerSnapshot,
};
pub use server::{
    DIRECT_IPC_SHUTDOWN_REASON, OWNER_BOUND_SHUTDOWN_REASON, OWNER_MISSING_ADMISSION_REASON,
    OWNER_WATCHER_ERROR_ADMISSION_REASON, OWNER_WATCHER_ERROR_SHUTDOWN_REASON, Server, ServerError,
    ServerIdentity, ServerLifecycleState, ServerMemorySnapshot, ServerRouteCloseOutcome,
    ServerShutdownCompletion,
};
