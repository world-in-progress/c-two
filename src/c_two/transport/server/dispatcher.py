"""Lightweight CRM method dispatcher using SimpleQueue.

Replaces ``ThreadPoolExecutor`` to avoid ``_WorkItem`` wrapping, heavy
``Queue`` locks, and per-submission ``Future`` creation overhead.

Worker threads dequeue ``(sched_execute, method, args, access, …)``
tuples, execute the CRM method under the scheduler's concurrency guard,
then build the reply frame and write it back to the client via
``loop.call_soon_threadsafe(writer.write, frame)``.
"""
from __future__ import annotations

import asyncio
import queue
import threading

from .reply import build_inline_reply, build_error_reply

_SENTINEL = None  # Poison pill for dispatcher shutdown


class FastDispatcher:
    """SimpleQueue-based worker pool for CRM method dispatch."""

    def __init__(self, loop: asyncio.AbstractEventLoop, num_workers: int = 2) -> None:
        self._loop = loop
        self._q: queue.SimpleQueue = queue.SimpleQueue()
        self._workers: list[threading.Thread] = []
        for i in range(num_workers):
            t = threading.Thread(
                target=self._worker, daemon=True, name=f'c2_fast_{i}',
            )
            t.start()
            self._workers.append(t)

    def submit(
        self,
        sched_execute,
        method,
        args_bytes,
        access,
        future: asyncio.Future,
    ) -> None:
        self._q.put((sched_execute, method, args_bytes, access, future, None, None, None, None))

    def submit_inline(
        self,
        sched_execute,
        method,
        args_bytes,
        access,
        request_id: int,
        writer,
        barrier=None,
        flight_dec=None,
    ) -> None:
        """Fire-and-forget: worker builds inline reply frame and writes directly.

        If *barrier* is an ``asyncio.Event``, the worker will call
        ``barrier.set()`` (via ``call_soon_threadsafe``) after execution
        completes, allowing the read loop to resume.  Used for ``@cc.write``
        methods to guarantee per-connection causal ordering.

        If *flight_dec* is provided, the worker will call it (via
        ``call_soon_threadsafe``) in the finally block to decrement the
        connection's in-flight counter.
        """
        self._q.put((sched_execute, method, args_bytes, access, None, request_id, writer, barrier, flight_dec))

    def shutdown(self) -> None:
        for _ in self._workers:
            self._q.put(_SENTINEL)
        for t in self._workers:
            t.join(timeout=2.0)

    def _worker(self) -> None:
        loop = self._loop
        q = self._q
        call_soon = loop.call_soon_threadsafe
        while True:
            item = q.get()
            if item is _SENTINEL:
                break
            sched_execute, method, args_bytes, access, future, request_id, writer, barrier, flight_dec = item
            try:
                result = sched_execute(method, args_bytes, access)
                if future is not None:
                    call_soon(future.set_result, result)
                else:
                    frame = build_inline_reply(request_id, result)
                    call_soon(writer.write, frame)
            except BaseException as exc:
                if future is not None:
                    call_soon(future.set_exception, exc)
                else:
                    frame = build_error_reply(request_id, exc)
                    call_soon(writer.write, frame)
            finally:
                if barrier is not None:
                    call_soon(barrier.set)
                if flight_dec is not None:
                    call_soon(flight_dec)
