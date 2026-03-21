import enum
import inspect
import logging
import threading
from concurrent.futures import Future, ThreadPoolExecutor
from contextlib import contextmanager
from dataclasses import dataclass, field
from typing import Any, Callable, Generic, Type, TypeVar

from .. import error
from ..crm.meta import MethodAccess, get_method_access
from .base import BaseServer
from .event import CompletionType, Event, EventQueue, EventTag
from .http import HttpServer
from .ipc import IPCv2Server
from .memory import MemoryServer
from .thread import ThreadServer
from .thread.thread_server import DirectCallEvent
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
    max_pending: int | None = None

    def __post_init__(self):
        object.__setattr__(self, 'mode', ConcurrencyMode(self.mode))
        if self.max_workers is not None and self.max_workers < 1:
            raise ValueError('max_workers must be at least 1 when provided.')
        if self.max_pending is not None and self.max_pending < 1:
            raise ValueError('max_pending must be at least 1 when provided.')


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
        self._pending_count = 0
        self._max_pending = config.max_pending
        self._drain_event = threading.Event()
        self._drain_event.set()

        effective_max_workers = config.max_workers
        if effective_max_workers is None and config.mode is ConcurrencyMode.EXCLUSIVE:
            effective_max_workers = 1

        self._executor = ThreadPoolExecutor(
            max_workers=effective_max_workers,
            thread_name_prefix='c_two_worker',
        )

    @property
    def pending_count(self) -> int:
        with self._state_lock:
            return self._pending_count

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
                raise error.CRMServerError('Server is shutting down, no new calls accepted.')

            if self._max_pending is not None and self._pending_count >= self._max_pending:
                raise error.CRMServerError(
                    f'Server is at capacity ({self._max_pending} pending calls). Try again later.'
                )

            self._pending_count += 1
            self._drain_event.clear()

        future = self._executor.submit(
            self._execute,
            method,
            args_bytes,
            get_method_access(method),
        )
        future.add_done_callback(
            lambda completed: self._on_task_done(
                request_id=request_id,
                future=completed,
                reply=reply,
            )
        )

    def submit_direct(
        self,
        *,
        request_id: str,
        crm_method: Callable,
        args: tuple,
        access_mode: MethodAccess,
        reply_direct: Callable[[str, Any, Exception | None], None],
    ) -> None:
        """Submit a direct call — CRM method receives Python objects, returns Python objects."""
        with self._state_lock:
            if self._shutdown:
                raise error.CRMServerError('Server is shutting down, no new calls accepted.')

            if self._max_pending is not None and self._pending_count >= self._max_pending:
                raise error.CRMServerError(
                    f'Server is at capacity ({self._max_pending} pending calls). Try again later.'
                )

            self._pending_count += 1
            self._drain_event.clear()

        future = self._executor.submit(self._execute_direct, crm_method, args, access_mode)
        future.add_done_callback(
            lambda completed: self._on_task_done_direct(
                request_id=request_id,
                future=completed,
                reply_direct=reply_direct,
            )
        )

    def shutdown(self) -> None:
        with self._state_lock:
            if self._shutdown:
                return
            self._shutdown = True

        self._drain_event.wait()
        self._executor.shutdown(wait=True)

    def _execute(
        self,
        method: Callable[[memoryview | bytes], bytes],
        args_bytes: memoryview | bytes,
        access_mode: MethodAccess,
    ) -> bytes:
        with self._execution_guard(access_mode):
            return method(args_bytes)

    def _execute_direct(
        self,
        crm_method: Callable,
        args: tuple,
        access_mode: MethodAccess,
    ) -> Any:
        with self._execution_guard(access_mode):
            return crm_method(*args)

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

    def _on_task_done(
        self,
        *,
        request_id: str,
        future: Future[bytes],
        reply: Callable[[Event], None],
    ) -> None:
        try:
            self._complete_request(request_id=request_id, future=future, reply=reply)
        finally:
            with self._state_lock:
                self._pending_count -= 1
                if self._pending_count == 0:
                    self._drain_event.set()

    def _on_task_done_direct(
        self,
        *,
        request_id: str,
        future: Future[Any],
        reply_direct: Callable[[str, Any, Exception | None], None],
    ) -> None:
        try:
            self._complete_direct_request(
                request_id=request_id, future=future, reply_direct=reply_direct
            )
        finally:
            with self._state_lock:
                self._pending_count -= 1
                if self._pending_count == 0:
                    self._drain_event.set()

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
            if isinstance(response, tuple):
                event = Event(
                    tag=EventTag.CRM_REPLY,
                    data_parts=list(response),
                    request_id=request_id,
                )
            else:
                if isinstance(response, memoryview):
                    response = response.tobytes()
                event = Event(tag=EventTag.CRM_REPLY, data=response, request_id=request_id)
        except Exception as exc:
            event = Event(
                tag=EventTag.CRM_REPLY,
                data=_serialize_server_error(exc),
                request_id=request_id,
            )

        try:
            reply(event)
        except Exception:
            logger.exception('Failed to deliver CRM reply for request %s', request_id)

    def _complete_direct_request(
        self,
        *,
        request_id: str,
        future: Future[Any],
        reply_direct: Callable[[str, Any, Exception | None], None],
    ) -> None:
        if future.cancelled():
            return

        try:
            result = future.result()
            reply_direct(request_id, result, None)
        except error.CCBaseError as exc:
            reply_direct(request_id, None, exc)
        except Exception as exc:
            reply_direct(request_id, None, error.CRMExecuteFunction(str(exc)))


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

    if isinstance(server, (ThreadServer, MemoryServer, HttpServer, IPCv2Server)):
        return

    protocol = server.bind_address.split('://', 1)[0]
    raise ValueError(
        f'Concurrency mode "{config.mode.value}" is not yet supported for "{protocol}://" servers. '
        'Use "thread://", "memory://", "http://", or "ipc-v2://" until the transport reply path is hardened.'
    )


def _stop_serving(state: ServerState) -> None:
    state.scheduler.shutdown()
    state.server.cancel_all_calls()
    state.server.destroy()

    for shutdown_event in state.shutdown_events:
        shutdown_event.set()

    state.stage = ServerStage.STOPPED


def _handle_shutdown(state: ServerState, event: Event) -> None:
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

        _stop_serving(state)


def _dispatch_crm_call(state: ServerState, event: Event) -> None:
    if state.stage is not ServerStage.STARTED:
        state.server.reply(
            Event(
                tag=EventTag.CRM_REPLY,
                data=_serialize_server_error(
                    error.CRMServerError('Server is shutting down, no new calls accepted.')
                ),
                request_id=event.request_id,
            )
        )
        return

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


def _dispatch_direct_call(state: ServerState, event: DirectCallEvent) -> None:
    """Dispatch a direct call — no serialization, Python objects passed directly to CRM."""
    if state.stage is not ServerStage.STARTED:
        state.server.reply_direct(
            event.request_id, None,
            error.CRMServerError('Server is shutting down, no new calls accepted.'),
        )
        return

    try:
        icrm = state.icrm
        crm = state.crm

        icrm_method = getattr(icrm, event.method_name, None)
        if icrm_method is None:
            raise error.CRMServerError(
                f'No method found: {icrm.__module__}.{icrm.__name__}.{event.method_name}'
            )

        crm_method = getattr(crm, event.method_name, None)
        if crm_method is None:
            raise error.CRMServerError(
                f'CRM method not found: {event.method_name}'
            )

        access_mode = get_method_access(icrm_method)
        state.scheduler.submit_direct(
            request_id=event.request_id,
            crm_method=crm_method,
            args=event.args,
            access_mode=access_mode,
            reply_direct=state.server.reply_direct,
        )
    except Exception as exc:
        wrapped = exc if isinstance(exc, error.CCBaseError) else error.CRMExecuteFunction(str(exc))
        state.server.reply_direct(event.request_id, None, wrapped)


def _process_event_and_continue(state: ServerState, event: Event | DirectCallEvent) -> bool:
    # Direct call fast path — no Event tag, no serialization
    if isinstance(event, DirectCallEvent):
        _dispatch_direct_call(state, event)
        return True

    if event.tag is EventTag.PING:
        state.server.reply(Event(tag=EventTag.PONG, request_id=event.request_id))
        return True

    if event.tag is EventTag.SHUTDOWN_FROM_CLIENT or event.tag is EventTag.SHUTDOWN_FROM_SERVER:
        _handle_shutdown(state, event)
        return False

    if event.tag is EventTag.CRM_CALL:
        _dispatch_crm_call(state, event)

    return True


def _serve(state: ServerState):
    while True:
        event = state.event_queue.poll(timeout=0.1)

        # DirectCallEvent has no completion_type — always a real event
        is_timeout = isinstance(event, Event) and event.completion_type == CompletionType.OP_TIMEOUT
        if not is_timeout:
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
        elif config.bind_address.startswith('ipc-v2://'):
            self.server = IPCv2Server(config.bind_address)
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
