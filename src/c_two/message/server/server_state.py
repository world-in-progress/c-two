import enum
import threading
from .base_server import BaseServer

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
    should_shutdown: threading.Event
    termination_event: threading.Event
    shutdown_events: list[threading.Event]

    def __init__(self, server: BaseServer, crm: object):
        self.crm = crm
        self.server = server
        
        self.lock = threading.RLock()
        self.stage = ServerStage.STOPPED
        self.should_shutdown = threading.Event()
        self.termination_event = threading.Event()
        self.shutdown_events = [self.termination_event]