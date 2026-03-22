import queue
import threading

from .event import Event, EventTag, CompletionType

class EventQueue:
    def __init__(self):
        self._queue = queue.Queue()
        self._shutdown = threading.Event()
    
    def poll(self, timeout: float | None = None) -> Event:
        if self._shutdown.is_set():
            return Event(
                tag=EventTag.SHUTDOWN_FROM_SERVER,
                completion_type=CompletionType.OP_COMPLETE
            )
        
        try:
            if timeout is None:
                event = self._queue.get(block=True)
            else:
                event = self._queue.get(block=True, timeout=timeout)
            
            if hasattr(event, 'completion_type'):
                event.completion_type = CompletionType.OP_COMPLETE
            return event
        except queue.Empty:
            return Event(
                data=None,
                tag=EventTag.EMPTY,
                completion_type=CompletionType.OP_TIMEOUT
            )

    def get(self) -> Event:
        """Block until an event is available.

        When ``shutdown()`` is called a sentinel envelope is placed into
        the queue so this method always unblocks promptly.
        """
        if self._shutdown.is_set():
            return Event(
                tag=EventTag.SHUTDOWN_FROM_SERVER,
                completion_type=CompletionType.OP_COMPLETE
            )

        event = self._queue.get(block=True)
        if hasattr(event, 'completion_type'):
            event.completion_type = CompletionType.OP_COMPLETE
        return event

    def put(self, event: Event) -> None:
        if not self._shutdown.is_set():
            self._queue.put(event)

    def shutdown(self) -> None:
        """Shutdown the event queue."""
        self._shutdown.set()
        # Put a sentinel so any blocking get() unblocks immediately
        from .envelope import Envelope
        from .msg_type import MsgType
        self._queue.put(Envelope(msg_type=MsgType.SHUTDOWN_SERVER))
        # Clear remaining items
        while not self._queue.empty():
            try:
                self._queue.get_nowait()
            except queue.Empty:
                break