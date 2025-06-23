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
            
            event.completion_type = CompletionType.OP_COMPLETE
            event.success = True
            return event
        except queue.Empty:
            return Event(
                data=None,
                tag=EventTag.EMPTY,
                completion_type=CompletionType.OP_TIMEOUT
            )

    def put(self, event: Event) -> None:
        if not self._shutdown.is_set():
            self._queue.put(event)

    def shutdown(self) -> None:
        """Shutdown the event queue."""
        self._shutdown.set()
        # Clear the queue
        while not self._queue.empty():
            try:
                self._queue.get_nowait()
            except queue.Empty:
                break