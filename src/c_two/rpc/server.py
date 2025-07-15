import enum
import logging
import threading

from .util.wait import wait
from .util.encoding import parse_message
from .event import Event, EventTag, EventQueue, CompletionType

from .base import BaseServer
from .zmq import ZmqServer
from .http import HttpServer
from .thread import ThreadServer
from .memory import MemoryServer

logger = logging.getLogger(__name__)

# Stage and State structures for Server ###############################

@enum.unique
class ServerStage(enum.Enum):
    STOPPED = "stopped"
    STARTED = "started"
    GRACE = "grace"

class ServerState(object):
    crm: object | None
    stage: ServerStage
    server: BaseServer
    lock: threading.RLock
    event_queue: EventQueue
    server_deallocated: bool
    termination_event: threading.Event
    shutdown_events: list[threading.Event]

    def __init__(self, server: BaseServer, event_queue: EventQueue, crm: object):
        self.crm = crm
        self.server = server
        self.event_queue = event_queue

        self.lock = threading.RLock()
        self.server_deallocated = False
        self.stage = ServerStage.STOPPED
        self.termination_event = threading.Event()
        self.shutdown_events = [self.termination_event]

# Common Server Operations ############################################

def _stop_serving(state: ServerState) -> bool:
    # Destroy the server
    state.server.cancel_all_calls()
    state.server.destroy()
    
    # Set the shutdown events to notify all waiting threads
    for shutdown_event in state.shutdown_events:
        shutdown_event.set()
        
    state.stage = ServerStage.STOPPED
    return True

def _process_event_and_continue(state: ServerState, event: Event) -> bool:
    """
    Process the received event and determine if server should continue running.
    
    Args:
        state (ServerState): The server state containing all necessary information.
        event (Event): The received event to process.
        
    Returns:
        bool: True if server should continue running, False if it should stop.
    """
    should_continue = True
    
    # Process PING event
    if event.tag is EventTag.PING:
        state.server.reply(Event(tag=EventTag.PONG, request_id=event.request_id))
    
    # Process SHUTDOWN event
    elif event.tag is EventTag.SHUTDOWN_FROM_CLIENT or event.tag is EventTag.SHUTDOWN_FROM_SERVER:
        with state.lock:
            # If the CRM instance has a terminate method, call it
            if state.crm and hasattr(state.crm, 'terminate'):
                state.crm.terminate()
                state.crm = None
            
            # Send shutdown acknowledgment if Shutdown event comes from client
            if event.tag is EventTag.SHUTDOWN_FROM_CLIENT:
                logger.info('Received shutdown request from client, shutting down server...')
                state.server.reply(Event(tag=EventTag.SHUTDOWN_ACK, request_id=event.request_id))
        
            if _stop_serving(state):
                should_continue = False
    
    # Process CRM_CALL event
    elif event.tag is EventTag.CRM_CALL:
        crm = state.crm
        sub_messages = parse_message(event.data)

        # Get method name
        method_name = sub_messages[0].tobytes().decode('utf-8')
        
        # Get arguments
        args_bytes = sub_messages[1]
        
        # Call method wrapped from CRM method
        method = getattr(crm, method_name)
        response = method.__wrapped__(crm, args_bytes)
        
        # Create a serialized response based on the serialized_error and serialized_result
        state.server.reply(Event(tag=EventTag.CRM_REPLY, data=response, request_id=event.request_id))

    return should_continue

def _serve(state: ServerState):
    while True:
        # Get the next event to process
        event = state.event_queue.poll(timeout=0.1)
        
        # Process the event and check if it is able to continue serving
        if event.completion_type != CompletionType.OP_TIMEOUT:
            if not _process_event_and_continue(state, event):
                return
        
        # Clear event to free memory
        event = None

def _begin_shutdown_once(state: ServerState) -> None:
    with state.lock:
        if state.stage is ServerStage.STARTED:
            state.server.shutdown()
            state.stage = ServerStage.GRACE

def _stop(state: ServerState) -> None:
    with state.lock:
        if state.stage is ServerStage.STOPPED:
            return
        
        _begin_shutdown_once(state)
        shutdown_event = threading.Event()
        state.shutdown_events.append(shutdown_event)

    shutdown_event.wait()
    return

def _start(state: ServerState) -> None:
    with state.lock:
        if state.stage is not ServerStage.STOPPED:
            raise RuntimeError('Cannot start already-started server.')
    
        state.server.start()
        state.stage = ServerStage.STARTED
        
        # Start serving in a daemon thread
        thread = threading.Thread(target=_serve, args=(state,))
        thread.daemon = True
        thread.start()

# Server Interface ####################################################

class Server:
    """
    A generic server interface that can handle different types of servers
    (ZMQ, HTTP, Memory) based on the provided bind address.
    """
    def __init__(self, bind_address: str, crm: object, name: str = ''):
        self.name = name if name != '' else 'CRM Server'
        
        # Check bind_address protocol and create appropriate server
        if bind_address.startswith(('tcp://', 'ipc://')):
            self.server = ZmqServer(bind_address)
        elif bind_address.startswith('http://'):
            self.server = HttpServer(bind_address)
        elif bind_address.startswith('memory://'):
            self.server = MemoryServer(bind_address)
        elif bind_address.startswith('thread://'):
            self.server = ThreadServer(bind_address)
        else:
            # TODO: Handle other protocols if needed
            raise ValueError(f'Unsupported protocol in bind_address: {bind_address}')
        
        # Create an event queue for the server
        event_queue = EventQueue()
        self.server.register_queue(event_queue)

        # Create the server state
        self._state = ServerState(
            crm=crm,
            server=self.server,
            event_queue=event_queue
        )
    
    def start(self) -> None:
        _start(self._state)
    
    def stop(self) -> None:
        _stop(self._state)
    
    def wait_for_termination(self, timeout: float | None = None) -> bool:
        return wait(
            self._state.termination_event.wait,
            self._state.termination_event.is_set,
            timeout=timeout
        )