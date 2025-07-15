import queue
import logging
import threading

from ... import error
from ..base import BaseServer
from ..event import Event, EventTag, EventQueue
from . import register_server, unregister_server

logger = logging.getLogger(__name__)

class ThreadServer(BaseServer):
    def __init__(self, bind_address: str, event_queue: EventQueue | None = None):
        super().__init__(bind_address, event_queue)
        
        # Extract thread_id from address like 'thread://cc_thread_12345'
        self.thread_id = bind_address.replace('thread://', '')
        
        # Thread-safe queues for communication
        self.request_queue: queue.Queue[Event] = queue.Queue()
        self.responses: dict[str, Event] = {}
        self.response_lock = threading.RLock()
        
        # Server state management
        self._shutdown_event = threading.Event()
        self._server_started = threading.Event()
        
        # Response notification mechanism
        self.response_condition: dict[str, threading.Condition] = {}
        self.conditions_lock = threading.Lock()
    
    def _serve(self):
        self._server_started.set()
        
        while True:
            try:
                if self._shutdown_event.is_set():
                    self.event_queue.put(Event(EventTag.SHUTDOWN_FROM_SERVER))
                    break
                
                # Wait for requests with timeout
                event = self.request_queue.get(timeout=0.1)
                
                if event.tag == EventTag.SHUTDOWN_FROM_CLIENT:
                    self.event_queue.put(Event(EventTag.SHUTDOWN_FROM_CLIENT))
                    break
                
                # Put event into the event queue for processing
                self.event_queue.put(event)
                
            except queue.Empty:
                continue
            except Exception as e:
                logger.error(f'Error in thread server loop: {e}')
                break
    
    def start(self):
        # Register this server in the global server registry
        register_server(self.thread_id, self)
        
        # Start the server thread
        server_thread = threading.Thread(target=self._serve)
        server_thread.daemon = True
        server_thread.start()
        
        # Wait for the server to start
        self._server_started.wait()
    
    def reply(self, event: Event) -> None:
        if not event.request_id:
            logger.warning('Reply event missing request_id')
            return
        
        # Store the response
        with self.response_lock:
            self.responses[event.request_id] = event
        
        # Notify waiting client
        with self.conditions_lock:
            condition = self.response_condition.get(event.request_id)
            if condition:
                with condition:
                    condition.notify()

    def shutdown(self):
        """Shutdown the thread server."""
        self._shutdown_event.set()
        
        # Unregister the server from the global registry
        unregister_server(self.thread_id)
    
    def destroy(self):
        # Clear all responses and conditions
        with self.response_lock:
            self.responses.clear()
        
        with self.conditions_lock:
            self.response_condition.clear()
    
    def cancel_all_calls(self):
        # Clear the request queue
        while not self.request_queue.empty():
            try:
                self.request_queue.get_nowait()
            except queue.Empty:
                break
        
        # Notify all waiting clients
        with self.conditions_lock:
            for condition in self.response_condition.values():
                with condition:
                    condition.notify_all()

    def put_request(self, event: Event):
        """Put a request into the server's queue (called by clients)."""
        try:
            self.request_queue.put(event)
        except queue.Full:
            raise error.CRMServerError(f'Request queue is full for thread server {self.thread_id}')
    
    def get_response(self, request_id: str, timeout: float = -1.0) -> Event | None:
        """Get a response for a specific request ID (called by clients)."""
        # Create a condition for this request if it doesn't exist
        with self.conditions_lock:
            if request_id not in self.response_condition:
                self.response_condition[request_id] = threading.Condition()
            condition = self.response_condition[request_id]
        
        # Wait for the response
        with condition:
            while request_id not in self.responses:
                if not condition.wait(timeout if timeout > 0 else None):
                    # Timeout reached
                    break
            
            # Check if the response is available
            with self.response_lock:
                response = self.responses.pop(request_id, None)
        
        # Clean up the condition
        with self.conditions_lock:
            self.response_condition.pop(request_id, None)
        
        return response