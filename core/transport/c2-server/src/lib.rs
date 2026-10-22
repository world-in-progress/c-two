mod admission;
mod catalog;
mod chunk_ordering;
pub mod config;
pub mod connection;
mod dispatcher;
pub mod heartbeat;
#[doc(hidden)]
pub mod owner_control;
pub mod response;
pub mod runtime;
mod scheduler;
pub mod server;

pub use admission::OwnerAdmissionControl;
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
    DIRECT_IPC_SHUTDOWN_REASON, OWNER_BOUND_SHUTDOWN_REASON, OWNER_MISSING_ADMISSION_REASON,
    OWNER_WATCHER_ERROR_ADMISSION_REASON, OWNER_WATCHER_ERROR_SHUTDOWN_REASON, Server, ServerError,
    ServerFailure, ServerIdentity, ServerLifecycleState, ServerMemorySnapshot, ServerRouteCloseOutcome,
    ServerShutdownCompletion,
};

pub use response::ResponsePool;
