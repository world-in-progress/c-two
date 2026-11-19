//! Read-only runtime diagnostics for monitoring and troubleshooting.
//! Includes connection events, call admission and concurrency snapshots, memory
//! statistics, Host lifecycle snapshots, and observed paths and routes.
//! Host completion is reported by `HostLifecycleSnapshot::finished`.

pub use c2_core::{
    ConnectionClosed, ConnectionEndReason, ConnectionEvent, ConnectionEvents,
    ConnectionEventsError, ConnectionObservation, ConnectionSide, HostLifecycleSnapshot,
    LocalCopyBucketStats, LocalCopyDirection, LocalCopyStats, MemoryCellStats, ObservedPath,
    PathCounters, RetiredCallExecutionReport, RetiredRoleObserver, RetiredRoleReport,
    RoleCallExecutionStats, RoleKind, RoleMemoryStats, RouteConcurrencySnapshot,
    RuntimeCallExecutionStats, RuntimeMemoryStats,
};
