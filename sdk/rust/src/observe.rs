//! Read-only runtime diagnostics for monitoring and troubleshooting.
//! Includes connection events, call admission and concurrency snapshots, memory
//! statistics, Host lifecycle snapshots, and observed paths and routes.
//! Host completion is reported by `HostLifecycleSnapshot::finished`.

pub use c2_core::{
    CallExecutionObservation, CallExecutionObserver, CallExecutionSnapshot, ConnectionClosed,
    ConnectionEndReason, ConnectionEvent, ConnectionEvents, ConnectionEventsError,
    ConnectionObservation, ConnectionSide, HostLifecycleSnapshot, MemoryCellStats,
    MemoryScopeStats, ObservedPath, PathCounters, RouteConcurrencySnapshot, RuntimeMemoryStats,
};
