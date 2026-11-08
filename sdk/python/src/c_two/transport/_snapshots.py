"""Typed shapes of the existing native runtime observations."""
from typing import Literal, TypedDict


class RouteCloseOutcome(TypedDict):
    route_name: str
    local_removed: bool
    active_drained: bool
    closed_reason: str
    close_error: str | None


class RelayCleanupError(TypedDict):
    route_name: str
    status_code: int | None
    message: str


class ClientCloseDetail(TypedDict):
    address: str
    reason: str


class ShutdownOutcome(TypedDict):
    removed_routes: list[str]
    route_outcomes: list[RouteCloseOutcome]
    relay_errors: list[RelayCleanupError]
    server_was_started: bool
    ipc_clients_drained: bool
    route_close_error: str | None
    runtime_barrier_error: str | None
    ipc_client_close_error: str | None
    ipc_client_close_details: list[ClientCloseDetail]
    completed: bool


class CallExecutionCounters(TypedDict):
    max_operations: int | None
    used_operations: int
    peak_operations: int
    rejected_reservations: int
    closed: bool


class RetiredCallExecutionSnapshot(CallExecutionCounters):
    initialized: bool
    state: Literal['retired']


class CallExecutionSnapshot(CallExecutionCounters):
    retired: list[RetiredCallExecutionSnapshot]


class LifecycleSnapshot(TypedDict):
    finished: bool
    listener_closed: bool
    work_drained: bool | None
