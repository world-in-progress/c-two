"""Runtime diagnostics for operators: connection events, memory, call execution and lifecycle."""
from ._native import (
    ConnectionClosed, ConnectionObservation, ConnectionEvent, ConnectionEvents,
    ConnectionEventsLagged,
)
from .transport._snapshots import CallExecutionSnapshot, LifecycleSnapshot, ShutdownOutcome
from .mem import MemoryStats, MemoryScopeStats, MemoryCellStats, MemoryLimits
from .transport.client.proxy import connection_observation
from .transport.registry import (
    connection_events, memory_stats, call_execution_snapshot, lifecycle_snapshot, shutdown_outcome,
)

__all__ = [
    'connection_events', 'connection_observation', 'ConnectionClosed', 'ConnectionObservation',
    'ConnectionEvent', 'ConnectionEvents', 'ConnectionEventsLagged', 'memory_stats',
    'MemoryStats', 'MemoryScopeStats', 'MemoryCellStats', 'MemoryLimits',
    'call_execution_snapshot', 'lifecycle_snapshot', 'shutdown_outcome',
    'CallExecutionSnapshot', 'LifecycleSnapshot', 'ShutdownOutcome',
]
