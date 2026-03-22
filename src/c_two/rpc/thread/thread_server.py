import queue
import logging
import threading
from dataclasses import dataclass, field
from typing import Any

from ... import error
from ..base import BaseServer
from ..event import Event, EventTag, EventQueue
from ..event.envelope import Envelope
from . import register_server, unregister_server

logger = logging.getLogger(__name__)

_SENTINEL = object()


@dataclass
class DirectCallEvent:
    """Direct call event for thread:// — carries Python objects, no serialization."""
    request_id: str
    method_name: str
    args: tuple


@dataclass
class _PendingResponse:
    ready: threading.Event
    response: Event | None = None


@dataclass
class _DirectPending:
    ready: threading.Event
    result: Any = _SENTINEL
    error: Exception | None = None


class ThreadServer(BaseServer):
    def __init__(self, bind_address: str, event_queue: EventQueue | None = None):
        super().__init__(bind_address, event_queue)
        
        # Extract thread_id from address like 'thread://cc_thread_12345'
        self.thread_id = bind_address.replace('thread://', '')
        
        self._pending_responses: dict[str, _PendingResponse] = {}
        self._direct_pending: dict[str, _DirectPending] = {}
        self._pending_lock = threading.RLock()
    
    def start(self):
        # Register this server in the global server registry
        register_server(self.thread_id, self)
    
    def reply(self, event: Event) -> None:
        if not event.request_id:
            logger.warning('Reply event missing request_id')
            return

        with self._pending_lock:
            pending = self._pending_responses.get(event.request_id)
            if pending is None:
                pending = _PendingResponse(ready=threading.Event())
                self._pending_responses[event.request_id] = pending

            pending.response = event
            pending.ready.set()

    def reply_direct(self, request_id: str, result: Any = None, err: Exception | None = None) -> None:
        """Deliver a direct Python object response (no serialization)."""
        with self._pending_lock:
            pending = self._direct_pending.get(request_id)
            if pending is None:
                pending = _DirectPending(ready=threading.Event())
                self._direct_pending[request_id] = pending

            pending.result = result
            pending.error = err
            pending.ready.set()

    def shutdown(self):
        """Shutdown the thread server."""
        # Unregister the server from the global registry
        unregister_server(self.thread_id)
    
    def destroy(self):
        with self._pending_lock:
            self._pending_responses.clear()
            self._direct_pending.clear()
    
    def cancel_all_calls(self):
        # Unregister the server from the global registry
        unregister_server(self.thread_id)
        
        # Clear the event queue
        self.event_queue.shutdown()
        
        # Notify all waiting clients (snapshot for free-threading safety)
        with self._pending_lock:
            for pending in list(self._pending_responses.values()):
                pending.ready.set()
            for pending in list(self._direct_pending.values()):
                pending.ready.set()

    def put_request(self, event: Event | DirectCallEvent | Envelope):
        """Put a request into the server's queue (called by clients)."""
        try:
            self.event_queue.put(event)
        except queue.Full:
            raise error.CRMServerError(f'Request queue is full for thread server {self.thread_id}')
    
    def get_response(self, request_id: str, timeout: float = -1.0) -> Event | None:
        """Get a response for a specific request ID (called by clients)."""
        with self._pending_lock:
            pending = self._pending_responses.get(request_id)
            if pending is None:
                pending = _PendingResponse(ready=threading.Event())
                self._pending_responses[request_id] = pending

            if pending.response is not None:
                response = pending.response
                self._pending_responses.pop(request_id, None)
                return response

            ready = pending.ready

        ready.wait(timeout if timeout > 0 else None)

        with self._pending_lock:
            pending = self._pending_responses.pop(request_id, None)
            if pending is None:
                return None
            return pending.response

    def get_direct_response(self, request_id: str, timeout: float = -1.0) -> tuple[Any, Exception | None]:
        """Get a direct Python object response (no deserialization)."""
        with self._pending_lock:
            pending = self._direct_pending.get(request_id)
            if pending is None:
                pending = _DirectPending(ready=threading.Event())
                self._direct_pending[request_id] = pending

            if pending.result is not _SENTINEL or pending.error is not None:
                result, err = pending.result, pending.error
                self._direct_pending.pop(request_id, None)
                return (None if result is _SENTINEL else result), err

            ready = pending.ready

        ready.wait(timeout if timeout > 0 else None)

        with self._pending_lock:
            pending = self._direct_pending.pop(request_id, None)
            if pending is None:
                return None, error.CompoClientError('No direct response received')
            result = None if pending.result is _SENTINEL else pending.result
            return result, pending.error
