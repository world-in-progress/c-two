import os
import signal
import socket
import subprocess
import tempfile
import hashlib
import uuid
from contextlib import contextmanager
from pathlib import Path
from collections.abc import Callable, Iterator
from typing import Any, TextIO
import urllib.request

# Disable .env loading before any c_two import — must be first.
os.environ['C2_ENV_FILE'] = ''

import pytest
import threading
import time
import c_two as cc

from c_two.transport.server import Server
from c_two.transport.client.util import ping

from tests.fixtures.hello import HelloImpl
from tests.fixtures.ihello import Hello

# Disable proxy for localhost to avoid HTTP test failures
os.environ.setdefault('NO_PROXY', '127.0.0.1,localhost')
os.environ.setdefault('no_proxy', '127.0.0.1,localhost')


# Unique address factory to avoid conflicts between tests
_address_counter = 0
_address_lock = threading.Lock()
_run_namespace = hashlib.sha256(
    (os.environ.get('CTWO_TEST_RUN_NAMESPACE')
     or os.environ.get('PYTEST_XDIST_TESTRUNUID')
     or uuid.uuid4().hex).encode()
).hexdigest()[:12]

def _next_id():
    global _address_counter
    with _address_lock:
        _address_counter += 1
        return _address_counter


@pytest.fixture
def unique_ipc_address() -> str:
    worker = os.environ.get('PYTEST_XDIST_WORKER', 'master')
    return f'ipc://test_hello_{_run_namespace}_{worker}_{os.getpid()}_{_next_id()}'


_SERIAL_MATRIX_FILES = (
    'test_portable_payload_cross_language.py',
    'test_portable_payload_matrix.py',
    'test_typescript_real_calls.py',
)


def pytest_configure(config: Any) -> None:
    """在controller创建workers前拒绝误用，让UsageError引导实际可见。"""
    if not (getattr(config.option, 'numprocesses', 0) or hasattr(config, 'workerinput')):
        return
    ignores = [Path(value).resolve() for value in (getattr(config.option, 'ignore', None) or [])]
    integration = Path(__file__).resolve().parent / 'integration'
    for name in _SERIAL_MATRIX_FILES:
        matrix = integration / name
        if any(matrix == ignored or ignored in matrix.parents for ignored in ignores):
            continue
        for argument in config.args:
            selected = Path(argument.split('::', 1)[0]).resolve()
            if selected == matrix or selected in matrix.parents:
                raise pytest.UsageError(
                    '完整portable/TypeScript矩阵必须单进程运行；使用 '
                    '`python tools/dev/test_python.py --help`编排，或对完整矩阵使用`-n0`。'
                )


def pytest_collection_finish(session: Any) -> None:
    """Build the real Rust parent before the per-test timeout starts."""
    if session.config.option.collectonly:
        return
    if any(Path(str(item.path)).name == 'test_owner_bound_lifecycle.py' for item in session.items):
        from tests.integration.test_owner_bound_lifecycle import _prepare_owner_launcher

        _prepare_owner_launcher()


def pytest_collection_modifyitems(config: Any, items: list[Any]) -> None:
    """矩阵依赖模块内完整累积收据，任何xdist分发都会破坏该契约。"""
    if not (getattr(config.option, 'numprocesses', 0) or hasattr(config, 'workerinput')):
        return
    if any(Path(str(item.path)).name in _SERIAL_MATRIX_FILES for item in items):
        raise pytest.UsageError(
            '完整portable/TypeScript矩阵必须单进程运行；使用 '
            '`python tools/dev/test_python.py --help`编排，或对完整矩阵使用`-n0`。'
        )


_relay_start_thread_lock = threading.Lock()


@contextmanager
def _relay_start_lock() -> Iterator[None]:
    """仅锁选端口至readiness，运行中已bind的relay可并存。

    xdist命名空间见官方how-to；锁必须跨worker，不能只是Python线程锁。
    不防御不遵守此fixture的外部进程抢占端口，既有启动失败断言仍有效。
    """
    uid = os.environ.get('PYTEST_XDIST_TESTRUNUID', _run_namespace)
    path = Path(os.environ.get(
        'CTWO_RELAY_START_LOCK',
        str(Path(tempfile.gettempdir()) / f'c2-relay-start-{hashlib.sha256(uid.encode()).hexdigest()[:24]}.lock'),
    ))
    with _relay_start_thread_lock:
        if os.name != 'posix':
            if os.environ.get('PYTEST_XDIST_WORKER'):
                raise RuntimeError('跨worker relay启动锁目前仅实现于Unix；Windows使用原有串行门禁')
            yield
            return
        import fcntl
        # 不unlink：避免等待者与新来者锁到不同inode。
        with path.open('a+b') as stream:
            fcntl.flock(stream, fcntl.LOCK_EX)
            try:
                yield
            finally:
                fcntl.flock(stream, fcntl.LOCK_UN)


@pytest.fixture(params=['ipc'])
def protocol_address(request, unique_ipc_address):
    """Parametrized fixture providing a unique address for each supported protocol."""
    addresses = {
        'ipc': unique_ipc_address,
    }
    return addresses[request.param]


@pytest.fixture
def hello_crm():
    """Create a Hello CRM instance."""
    return HelloImpl()


def _wait_for_server(address: str, timeout: float = 5.0) -> None:
    """Poll until the server responds to ping."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if ping(address, timeout=0.5):
                return
        except Exception:
            pass
        time.sleep(0.05)
    raise TimeoutError(f'Server at {address} not ready after {timeout}s')


def repo_root() -> Path:
    return Path(__file__).resolve().parents[3]


def free_tcp_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(('127.0.0.1', 0))
        return int(sock.getsockname()[1])


def c3_binary() -> Path:
    selected = os.environ.get('CTWO_TEST_C3_BIN')
    if selected:
        candidate = Path(selected)
        if not candidate.is_file():
            raise AssertionError(f'选定预建c3不存在：{candidate}')
        return candidate
    root = repo_root()
    name = 'c3.exe' if os.name == 'nt' else 'c3'
    candidates = [
        root / 'cli' / 'target' / 'debug' / name,
        root / 'cli' / 'target' / 'release' / name,
    ]
    for candidate in candidates:
        if candidate.exists():
            return candidate
    pytest.skip(
        'c3 binary is required for relay tests. '
        'Run `python tools/dev/c3_tool.py --build --link` from the repository root.'
    )


def wait_for_relay(
    url: str,
    timeout: float = 5.0,
    proc: subprocess.Popen[str] | None = None,
) -> None:
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc is not None and proc.poll() is not None:
            raise RuntimeError(f'Relay process exited with code {proc.returncode}')
        try:
            with opener.open(f'{url}/health', timeout=0.5) as resp:
                if resp.status == 200:
                    return
        except Exception:
            pass
        time.sleep(0.1)
    raise TimeoutError(f'Relay at {url} not ready after {timeout}s')


def stop_process(proc: subprocess.Popen[str]) -> None:
    if proc.poll() is not None:
        return
    if os.name == 'nt':
        proc.terminate()
    else:
        proc.send_signal(signal.SIGINT if hasattr(signal, 'SIGINT') else signal.SIGTERM)
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


class RelayProcess:
    def __init__(
        self,
        proc: subprocess.Popen[str],
        url: str,
        bind: str,
        stdout_log: TextIO,
        stderr_log: TextIO,
    ):
        self.proc = proc
        self.url = url
        self.bind = bind
        self.stdout_log = stdout_log
        self.stderr_log = stderr_log

    def stop(self) -> None:
        try:
            stop_process(self.proc)
        finally:
            self.stdout_log.close()
            self.stderr_log.close()


def _read_log(log: TextIO) -> str:
    log.flush()
    log.seek(0)
    return log.read()


def _open_process_log(channel: str = 'process') -> TextIO:
    stage = os.environ.get('CTWO_PYTHON_STAGE_DIR')
    if stage:
        directory = Path(stage) / 'relay-logs'
        directory.mkdir(parents=True, exist_ok=True)
        worker = os.environ.get('PYTEST_XDIST_WORKER', 'master')
        path = directory / f'{worker}-{os.getpid()}-{uuid.uuid4().hex}-{channel}.log'
        log = path.open('w+', encoding='utf-8')
        log.write(f"pytest: {os.environ.get('PYTEST_CURRENT_TEST', 'unknown')}\n")
        log.flush()
        return log
    return tempfile.TemporaryFile(mode='w+', encoding='utf-8')


@pytest.fixture
def start_c3_relay() -> Iterator[Callable[..., RelayProcess]]:
    processes: list[RelayProcess] = []

    def _launch(
        *,
        actual_port: int,
        relay_id: str | None = None,
        seeds: list[str] | None = None,
        idle_timeout: int | None = None,
    ) -> RelayProcess:
        bind = f'127.0.0.1:{actual_port}'
        url = f'http://127.0.0.1:{actual_port}'
        args = [str(c3_binary()), 'relay', '--bind', bind, '--advertise-url', url]
        if relay_id is not None:
            args.extend(['--relay-id', relay_id])
        if seeds:
            args.extend(['--seeds', ','.join(seeds)])
        if idle_timeout is not None:
            args.extend(['--idle-timeout', str(idle_timeout)])

        env = os.environ.copy()
        env['C2_ENV_FILE'] = ''
        env['NO_PROXY'] = '127.0.0.1,localhost'
        env['no_proxy'] = '127.0.0.1,localhost'
        stdout_log = _open_process_log('stdout')
        stderr_log = _open_process_log('stderr')
        try:
            proc = subprocess.Popen(
                args,
                cwd=repo_root(),
                env=env,
                stdout=stdout_log,
                stderr=stderr_log,
                text=True,
            )
        except Exception:
            stdout_log.close()
            stderr_log.close()
            raise
        relay = RelayProcess(
            proc=proc,
            url=url,
            bind=bind,
            stdout_log=stdout_log,
            stderr_log=stderr_log,
        )
        try:
            wait_for_relay(url, proc=proc)
        except Exception:
            stop_process(proc)
            stdout = _read_log(stdout_log)
            stderr = _read_log(stderr_log)
            stdout_log.close()
            stderr_log.close()
            raise AssertionError(
                f'c3 relay failed to start\nstdout:\n{stdout}\nstderr:\n{stderr}'
            )
        processes.append(relay)
        return relay

    def _start_locked(
        *,
        port: int | None = None,
        relay_id: str | None = None,
        seeds: list[str] | None = None,
        idle_timeout: int | None = None,
    ) -> RelayProcess:
        last_error: AssertionError | None = None
        attempts = 1 if port is not None else 5
        for _ in range(attempts):
            actual_port = port if port is not None else free_tcp_port()
            try:
                return _launch(
                    actual_port=actual_port,
                    relay_id=relay_id,
                    seeds=seeds,
                    idle_timeout=idle_timeout,
                )
            except AssertionError as exc:
                last_error = exc
                if port is not None:
                    raise
        assert last_error is not None
        raise last_error

    def _start(
        *,
        port: int | None = None,
        relay_id: str | None = None,
        seeds: list[str] | None = None,
        idle_timeout: int | None = None,
    ) -> RelayProcess:
        with _relay_start_lock():
            return _start_locked(port=port, relay_id=relay_id, seeds=seeds, idle_timeout=idle_timeout)

    yield _start

    cleanup_errors: list[Exception] = []
    for relay in reversed(processes):
        try:
            relay.stop()
        except Exception as exc:
            cleanup_errors.append(exc)
    if cleanup_errors:
        raise AssertionError(f'failed to stop {len(cleanup_errors)} c3 relay process(es)')


@pytest.fixture
def hello_server(protocol_address, hello_crm):
    """Start a Hello CRM server on the given protocol, yield the address, then shut down."""
    server = Server(
        bind_address=protocol_address,
        crm_class=Hello,
        crm_instance=hello_crm,
    )
    server.start()
    _wait_for_server(protocol_address)

    yield protocol_address

    server.shutdown()
