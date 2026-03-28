"""Read/write-aware task scheduler for CRM method execution.

Extracted and simplified from :mod:`c_two.rpc.server._Scheduler`.

Three concurrency modes:

- **EXCLUSIVE** (default): all CRM calls serialized via a single lock.
- **READ_PARALLEL**: ``@cc.read`` methods run concurrently; ``@cc.write``
  methods acquire exclusive access (writer-priority RW lock).
- **PARALLEL**: no locking — the CRM is assumed fully thread-safe.

Usage with asyncio (ServerV2)::

    sched = Scheduler(ConcurrencyConfig(mode='read_parallel'))
    loop = asyncio.get_running_loop()
    result = await loop.run_in_executor(
        sched.executor,
        sched.execute, method, args_bytes, access_mode,
    )

The scheduler owns the :class:`~concurrent.futures.ThreadPoolExecutor` and
tracks pending call count for graceful drain on shutdown.
"""
from __future__ import annotations

import enum
import threading
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from dataclasses import dataclass
from typing import Any, Callable

# Re-export access decorators so consumers only import from rpc_v2.
from ..crm.meta import MethodAccess, get_method_access  # noqa: F401


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

@enum.unique
class ConcurrencyMode(enum.Enum):
    EXCLUSIVE = 'exclusive'
    READ_PARALLEL = 'read_parallel'
    PARALLEL = 'parallel'


@dataclass(frozen=True)
class ConcurrencyConfig:
    mode: ConcurrencyMode | str = ConcurrencyMode.EXCLUSIVE
    max_workers: int | None = None
    max_pending: int | None = None

    def __post_init__(self) -> None:
        object.__setattr__(self, 'mode', ConcurrencyMode(self.mode))
        if self.max_workers is not None and self.max_workers < 1:
            raise ValueError('max_workers must be at least 1 when provided.')
        if self.max_pending is not None and self.max_pending < 1:
            raise ValueError('max_pending must be at least 1 when provided.')


# ---------------------------------------------------------------------------
# Writer-priority read/write lock
# ---------------------------------------------------------------------------

class _WriterPriorityReadWriteLock:
    """Readers run concurrently; writers get exclusive access with priority.

    When a writer is waiting, new readers block until the writer completes.
    """

    def __init__(self) -> None:
        self._condition = threading.Condition()
        self._active_readers = 0
        self._writer_active = False
        self._waiting_writers = 0

    @contextmanager
    def read_lock(self):
        with self._condition:
            while self._writer_active or self._waiting_writers > 0:
                self._condition.wait()
            self._active_readers += 1
        try:
            yield
        finally:
            with self._condition:
                self._active_readers -= 1
                if self._active_readers == 0:
                    self._condition.notify_all()

    @contextmanager
    def write_lock(self):
        with self._condition:
            self._waiting_writers += 1
            try:
                while self._writer_active or self._active_readers > 0:
                    self._condition.wait()
                self._writer_active = True
            finally:
                self._waiting_writers -= 1
        try:
            yield
        finally:
            with self._condition:
                self._writer_active = False
                self._condition.notify_all()


# ---------------------------------------------------------------------------
# Scheduler
# ---------------------------------------------------------------------------

class Scheduler:
    """Thread-pool scheduler with read/write concurrency control.

    Parameters
    ----------
    config:
        Concurrency mode and worker limits.
    """

    def __init__(self, config: ConcurrencyConfig | None = None) -> None:
        self._config = config or ConcurrencyConfig()
        self._exclusive_lock = threading.Lock()
        self._rw_lock = _WriterPriorityReadWriteLock()

        # Pending-call tracking for graceful drain.
        self._state_lock = threading.Lock()
        self._shutdown_flag = False
        self._pending_count = 0
        self._drain_event = threading.Event()
        self._drain_event.set()
        # Pre-compute hot-path flags.
        self._has_capacity_limit = config is not None and config.max_pending is not None
        self._is_exclusive = self._config.mode is ConcurrencyMode.EXCLUSIVE
        self._is_parallel = self._config.mode is ConcurrencyMode.PARALLEL

        effective_workers = self._config.max_workers
        if effective_workers is None and self._config.mode is ConcurrencyMode.EXCLUSIVE:
            effective_workers = 1

        # Executor kept for backward compatibility (v1 dispatch path)
        # but _FastDispatcher is the primary dispatch mechanism.
        self._executor: ThreadPoolExecutor | None = None
        self._executor_workers = effective_workers

    @property
    def executor(self) -> ThreadPoolExecutor:
        """The underlying executor — lazy creation for backward compatibility."""
        if self._executor is None:
            self._executor = ThreadPoolExecutor(
                max_workers=self._executor_workers,
                thread_name_prefix='c2v2_worker',
            )
        return self._executor

    @property
    def pending_count(self) -> int:
        with self._state_lock:
            return self._pending_count

    # ------------------------------------------------------------------
    # Synchronous execution (called from within run_in_executor)
    # ------------------------------------------------------------------

    def execute(
        self,
        method: Callable[..., Any],
        args_bytes: bytes | memoryview,
        access_mode: MethodAccess = MethodAccess.WRITE,
    ) -> Any:
        """Execute *method(args_bytes)* with the appropriate concurrency guard."""
        try:
            if self._is_parallel:
                return method(args_bytes)
            if self._is_exclusive:
                with self._exclusive_lock:
                    return method(args_bytes)
            # READ_PARALLEL
            if access_mode is MethodAccess.READ:
                with self._rw_lock.read_lock():
                    return method(args_bytes)
            with self._rw_lock.write_lock():
                return method(args_bytes)
        finally:
            with self._state_lock:
                self._pending_count -= 1
                if self._shutdown_flag and self._pending_count == 0:
                    self._drain_event.set()

    def execute_fast(
        self,
        method: Callable[..., Any],
        args_bytes: bytes | memoryview,
        access_mode: MethodAccess = MethodAccess.WRITE,
    ) -> Any:
        """Like execute() but without pending-count tracking.

        Used by _FastDispatcher where task lifecycle is managed by asyncio.
        Caller must ensure shutdown() waits for tasks via other means.
        """
        if self._is_parallel:
            return method(args_bytes)
        if self._is_exclusive:
            with self._exclusive_lock:
                return method(args_bytes)
        if access_mode is MethodAccess.READ:
            with self._rw_lock.read_lock():
                return method(args_bytes)
        with self._rw_lock.write_lock():
            return method(args_bytes)

    def begin(self) -> None:
        """Increment pending counter before dispatching to the executor."""
        if self._shutdown_flag:
            raise RuntimeError('Scheduler is shut down')
        if self._has_capacity_limit:
            with self._state_lock:
                if (self._pending_count >= self._config.max_pending):
                    raise RuntimeError(
                        f'Scheduler at capacity ({self._config.max_pending} pending)',
                    )
                self._pending_count += 1
        else:
            with self._state_lock:
                self._pending_count += 1

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def shutdown(self) -> None:
        """Wait for pending calls to drain, then shut down the executor."""
        with self._state_lock:
            if self._shutdown_flag:
                return
            self._shutdown_flag = True
        self._drain_event.wait()
        if self._executor is not None:
            self._executor.shutdown(wait=True)

    # ------------------------------------------------------------------
    # Internal
    # ------------------------------------------------------------------

    @contextmanager
    def execution_guard(self, access_mode: MethodAccess):
        if self._config.mode is ConcurrencyMode.EXCLUSIVE:
            with self._exclusive_lock:
                yield
            return

        if self._config.mode is ConcurrencyMode.READ_PARALLEL:
            if access_mode is MethodAccess.READ:
                with self._rw_lock.read_lock():
                    yield
            else:
                with self._rw_lock.write_lock():
                    yield
            return

        # PARALLEL — no locking.
        yield
