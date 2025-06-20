import logging
import threading
from . import common
from .tcp_server import TcpServer
from ..event import Event, EventTag
from ..util.encoding import parse_message
from .server_state import ServerState, ServerStage

# Logging Configuration ###########################################################
logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)

def _stop_serving(state: ServerState) -> bool:
    with state.lock:
        # Destroy the server
        state.server.cancel_all_calls()
        state.server.destroy()
        
        # Set the shutdown events to notify all waiting threads
        for shutdown_event in state.shutdown_events:
            shutdown_event.set()
            
        state.stage = ServerStage.STOPPED
        return True

def _process_event_and_continue(state: ServerState, event: Event | None) -> bool:
    """
    Process the received event and return the response.
    
    Args:
        event (Event): The received event.
        crm_instance (object): The CRM instance to call the method on.
        
    Returns:
        Event: The response event.
    """
    # No event to process, continue serving
    if event is None:
        return True
    
    # Process PING event
    if event.tag is EventTag.PING:
        state.server.reply(Event(EventTag.PONG, None))
        return True
    
    # Process SHUTDOWN event
    if event.tag is EventTag.SHUTDOWN_FROM_CLIENT or event.tag is EventTag.SHUTDOWN_FROM_SERVER:
        # If the CRM instance has a terminate method, call it
        if state.crm and hasattr(state.crm, 'terminate'):
            state.crm.terminate()
            state.crm = None
        
        # Send shutdown acknowledgment if Shutdown event comes from client
        if event.tag is EventTag.SHUTDOWN_FROM_CLIENT:
            state.server.reply(Event(EventTag.SHUTDOWN_ACK, None))
            
        return not _stop_serving(state)
        
    # Process CRM_CALL event
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
    state.server.reply(Event(EventTag.CRM_REPLY, response))
    
    return True

def _serve(state: ServerState):
    while True:
        # Get the next event to process
        if state.should_shutdown.is_set():
            event = Event(EventTag.SHUTDOWN_FROM_SERVER, None)
        else:
            event = state.server.pool(timeout=0.1)
        
        # Process the event and check if it is able to continue serving
        if not _process_event_and_continue(state, event):
            break
        
        # Clear event to free memory
        event = None

def _begin_shutdown_once(state: ServerState) -> None:
    with state.lock:
        if state.stage is ServerStage.STARTED:
            state.server.shutdown()
            state.should_shutdown.set()
            state.stage = ServerStage.GRACE

def _stop(state: ServerState) -> None:
    with state.lock:
        shutdown_event = threading.Event()
        state.shutdown_events.append(shutdown_event)
        _begin_shutdown_once(state)

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

class Server:
    _state: ServerState
    
    def __init__(self, bind_address: str, crm: object, name: str = '', timeout: int = 0):
        self.name = name if name != '' else 'CRM Server'
        
        # Check bind_address use TCP or IPC protocol
        if bind_address.startswith(('tcp://', 'ipc://')):
            self.server = TcpServer(bind_address)
        else:
            # TODO: Handle other protocols if needed
            pass
        
        self._state = ServerState(
            server=self.server,
            crm=crm
        )
    
    def start(self) -> None:
        _start(self._state)
    
    def stop(self) -> None:
        _stop(self._state)
    
    def wait_for_termination(self, timeout: float | None = None) -> bool:
        return common.wait(
            self._state.termination_event.wait,
            self._state.termination_event.is_set,
            timeout=timeout
        )