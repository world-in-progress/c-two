import enum
import threading
from .base import BaseServer
from .event_queue import EventQueue

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