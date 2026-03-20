import enum
import inspect
import logging
import threading
from concurrent.futures import Future, ThreadPoolExecutor
from contextlib import contextmanager
from dataclasses import dataclass, field
from typing import Callable, Generic, Type, TypeVar

from .. import error
from ..crm.meta import MethodAccess, get_method_access
from .base import BaseServer
from .event import CompletionType, Event, EventQueue, EventTag
from .http import HttpServer
from .memory import MemoryServer
from .thread import ThreadServer
from .util.encoding import add_length_prefix, parse_message
from .util.wait import wait
from .zmq import ZmqServer

CRM = TypeVar('CRM')
ICRM = TypeVar('ICRM')
logger = logging.getLogger(__name__)


@enum.unique
class ConcurrencyMode(enum.Enum):
    EXCLUSIVE = 'exclusive'
    READ_PARALLEL = 'read_parallel'
    PARALLEL = 'parallel'


@dataclass(frozen=True)
class ConcurrencyConfig:
    mode: ConcurrencyMode | str = ConcurrencyMode.EXCLUSIVE
    max_workers: int | None = None

    def __post_init__(self):
        object.__setattr__(self, 'mode', ConcurrencyMode(self.mode))
        if self.max_workers is not None and self.max_workers < 1:
            raise ValueError('max_workers must be at least 1 when provided.')


@dataclass
class ServerConfig(Generic[CRM, ICRM]):
    crm: CRM
    icrm: Type[ICRM]
    bind_address: str
    name: str = ''
    on_shutdown: callable = lambda: None
    concurrency: ConcurrencyConfig = field(default_factory=ConcurrencyConfig)

    def __post_init__(self):
        """
        Set default name if not provided.
        Comprehensive validation to ensure CRM fully supports ICRM interface.
        """
        if not self.name:
            self.name = f'{self.crm.__class__.__name__}'

        if not isinstance(self.concurrency, ConcurrencyConfig):
            raise TypeError('ServerConfig.concurrency must be a ConcurrencyConfig instance.')

        icrm_methods = {
            name: method
            for name, method in inspect.getmembers(self.icrm, predicate=inspect.isfunction)
            if not name.startswith('_')
        }

        crm_methods = {
            name: method
            for name, method in inspect.getmembers(self.crm.__class__, predicate=inspect.isfunction)
            if not name.startswith('_')
        }

        missing_methods = set(icrm_methods.keys()) - set(crm_methods.keys())
        if missing_methods:
            raise ValueError(f'The CRM instance is missing implementations for methods: {missing_methods}')


@enum.unique
class ServerStage(enum.Enum):
    GRACE = 'grace'
    STOPPED = 'stopped'
    STARTED = 'started'


class _WriterPriorityReadWriteLock:
    def __init__(self):
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


class _Scheduler:
    def __init__(self, config: ConcurrencyConfig):
        self._config = config
        self._exclusive_lock = threading.Lock()
        self._rw_lock = _WriterPriorityReadWriteLock()
        self._state_lock = threading.Lock()
        self._shutdown = False

        effective_max_workers = config.max_workers
        if effective_max_workers is None and config.mode is ConcurrencyMode.EXCLUSIVE:
            effective_max_workers = 1

        self._executor = ThreadPoolExecutor(
            max_workers=effective_max_workers,
            thread_name_prefix='c_two_worker',
        )

    def submit(
        self,
        *,
        request_id: str,
        method: Callable[[memoryview | bytes], bytes],
        args_bytes: memoryview | bytes,
        reply: Callable[[Event], None],
    ) -> None:
        with self._state_lock:
            if self._shutdown:
                raise RuntimeError('Cannot schedule CRM calls after scheduler shutdown has started.')

        future = self._executor.submit(
            self._execute,
            method,
            args_bytes,
            get_method_access(method),
        )
        future.add_done_callback(
            lambda completed: self._complete_request(
                request_id=request_id,
                future=completed,
                reply=reply,
            )
        )

    def shutdown(self) -> None:
        with self._state_lock:
            if self._shutdown:
                return
            self._shutdown = True

        self._executor.shutdown(wait=True)

    def _execute(
        self,
        method: Callable[[memoryview | bytes], bytes],
        args_bytes: memoryview | bytes,
        access_mode: MethodAccess,
    ) -> bytes:
        with self._execution_guard(access_mode):
            return method(args_bytes)

    @contextmanager
    def _execution_guard(self, access_mode: MethodAccess):
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

        yield

    def _complete_request(
        self,
        *,
        request_id: str,
        future: Future[bytes],
        reply: Callable[[Event], None],
    ) -> None:
        if future.cancelled():
            return

        try:
            response = future.result()
            if isinstance(response, memoryview):
                response = response.tobytes()
        except Exception as exc:
            response = _serialize_server_error(exc)

        try:
            reply(Event(tag=EventTag.CRM_REPLY, data=response, request_id=request_id))
        except Exception:
            logger.exception('Failed to deliver CRM reply for request %s', request_id)


class ServerState:
    crm: CRM
    icrm: ICRM
    stage: ServerStage
    server: BaseServer
    scheduler: _Scheduler
    on_shutdown: callable
    lock: threading.RLock
    event_queue: EventQueue
    server_deallocated: bool
    termination_event: threading.Event
    shutdown_events: list[threading.Event]

    def __init__(
        self,
        server: BaseServer,
        scheduler: _Scheduler,
        event_queue: EventQueue,
        crm: CRM,
        icrm: ICRM,
        on_shutdown: callable,
    ):
        self.crm = crm
        self.icrm = icrm
        self.server = server
        self.scheduler = scheduler
        self.event_queue = event_queue
        self.on_shutdown = on_shutdown

        self.lock = threading.RLock()
        self.server_deallocated = False
        self.stage = ServerStage.STOPPED
        self.termination_event = threading.Event()
        self.shutdown_events = [self.termination_event]


def _serialize_server_error(exc: Exception) -> bytes:
    server_error = exc if isinstance(exc, error.CCError) else error.CRMServerError(str(exc))
    serialized_error = error.CCError.serialize(server_error)
    return add_length_prefix(serialized_error) + add_length_prefix(b'')


def _validate_concurrency_support(server: BaseServer, config: ConcurrencyConfig) -> None:
    if config.mode is ConcurrencyMode.EXCLUSIVE:
        return

    if isinstance(server, (ThreadServer, MemoryServer)):
        return

    protocol = server.bind_address.split('://', 1)[0]
    raise ValueError(
        f'Concurrency mode "{config.mode.value}" is not yet supported for "{protocol}://" servers. '
        'Use "thread://" or "memory://" until the transport reply path is hardened.'
    )


def _stop_serving(state: ServerState) -> bool:
    state.scheduler.shutdown()
    state.server.cancel_all_calls()
    state.server.destroy()

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

    if event.tag is EventTag.PING:
        state.server.reply(Event(tag=EventTag.PONG, request_id=event.request_id))

    elif event.tag is EventTag.SHUTDOWN_FROM_CLIENT or event.tag is EventTag.SHUTDOWN_FROM_SERVER:
        with state.lock:
            if state.on_shutdown:
                try:
                    state.on_shutdown()
                    state.on_shutdown = None
                    state.crm = None
                except Exception as exc:
                    logger.error(f'Error during on_shutdown callback: {exc}')

            if event.tag is EventTag.SHUTDOWN_FROM_CLIENT:
                logger.info('Received shutdown request from client, shutting down server...')
                state.server.reply(Event(tag=EventTag.SHUTDOWN_ACK, request_id=event.request_id))

            if _stop_serving(state):
                should_continue = False

    elif event.tag is EventTag.CRM_CALL:
        try:
            icrm = state.icrm
            sub_messages = parse_message(event.data)

            method_name = sub_messages[0].tobytes().decode('utf-8')
            args_bytes = sub_messages[1]

            method = getattr(icrm, method_name, None)
            if method is None:
                raise error.CRMServerError(
                    f'No wrapped function found for method: {icrm.__module__}.{icrm.__name__}.{method_name}'
                )

            state.scheduler.submit(
                request_id=event.request_id,
                method=method,
                args_bytes=args_bytes,
                reply=state.server.reply,
            )
        except Exception as exc:
            state.server.reply(
                Event(
                    tag=EventTag.CRM_REPLY,
                    data=_serialize_server_error(exc),
                    request_id=event.request_id,
                )
            )

    return should_continue


def _serve(state: ServerState):
    while True:
        event = state.event_queue.poll(timeout=0.1)

        if event.completion_type != CompletionType.OP_TIMEOUT:
            if not _process_event_and_continue(state, event):
                return

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

        thread = threading.Thread(target=_serve, args=(state,))
        thread.daemon = True
        thread.start()


class Server:
    """
    A generic server interface that can handle different types of servers
    (ZMQ, HTTP, Memory) based on the provided bind address.
    """

    def __init__(self, config: ServerConfig):
        self.name = config.name

        if config.bind_address.startswith(('tcp://', 'ipc://')):
            self.server = ZmqServer(config.bind_address)
        elif config.bind_address.startswith('http://'):
            self.server = HttpServer(config.bind_address)
        elif config.bind_address.startswith('memory://'):
            self.server = MemoryServer(config.bind_address)
        elif config.bind_address.startswith('thread://'):
            self.server = ThreadServer(config.bind_address)
        else:
            raise ValueError(f'Unsupported protocol in bind_address: {config.bind_address}')

        _validate_concurrency_support(self.server, config.concurrency)

        event_queue = EventQueue()
        self.server.register_queue(event_queue)

        icrm = config.icrm()
        icrm.crm = config.crm
        icrm.direction = '<-'

        self._state = ServerState(
            icrm=icrm,
            crm=config.crm,
            server=self.server,
            scheduler=_Scheduler(config.concurrency),
            on_shutdown=config.on_shutdown,
            event_queue=event_queue,
        )

    def start(self, timeout: float | None = None) -> None:
        _start(self._state)
        logger.info(f'CRM server "{self.name}" is running. Press Ctrl+C to stop.')

        try:
            self.wait_for_termination(timeout)
            logger.info(f'Timeout reached, stopping CRM server "{self.name}"...')

        except KeyboardInterrupt:
            logger.info(f'Stopping CRM server "{self.name}"...')

        finally:
            self.stop()
            logger.info(f'CRM server "{self.name}" stopped.')

    def stop(self) -> None:
        _stop(self._state)

    def wait_for_termination(self, timeout: float | None = None) -> bool:
        return wait(
            self._state.termination_event.wait,
            self._state.termination_event.is_set,
            timeout=timeout,
        )
