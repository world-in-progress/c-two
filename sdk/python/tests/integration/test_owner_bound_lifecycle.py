"""Real controller/service ownership integration test for the Python facade.

The controller is a real Rust SDK parent process that creates the private owner
control pair, starts the service child with the receiver as that child's
stdin, and holds the keepalive. The service is a real Python resource process
that explicitly adopts the inherited receiver, arms an ``owner_bound`` host,
and blocks in ``cc.serve()``.

The controller's stdout carries only progress markers: the capability is never
printed, and the service's ``@on_shutdown`` must run exactly once when the
controller releases (or dies).
"""
from __future__ import annotations

import os
import queue
import subprocess
import sys
import textwrap
import threading
import time
from pathlib import Path

import pytest
from fastdb4py.payload import Payload

from tests.fixtures.process_control import python_process_options

FIXTURE = Path(__file__).resolve().parents[1] / 'fixtures' / 'owner_bound_service.py'
REPOSITORY_ROOT = Path(__file__).resolve().parents[4]

READY_MARKER = 'OWNER_BOUND_READY'
FINISHED_MARKER = 'OWNER_BOUND_FINISHED'
HOLDING_MARKER = 'OWNER_BOUND_HOLDING'
SPAWNED_MARKER = 'CONTROLLER_SPAWNED'
RELEASED_MARKER = 'CONTROLLER_RELEASED'


def _subprocess_env(tmp_path: Path, marker: Path, **extra: str) -> dict[str, str]:
    env = os.environ.copy()
    env.pop('C2_RELAY_ANCHOR_ADDRESS', None)
    env['PYTHONUNBUFFERED'] = '1'
    env['PYTHONPATH'] = os.pathsep.join(
        [str(REPOSITORY_ROOT / 'sdk' / 'python' / 'src'), env.get('PYTHONPATH', '')],
    ).strip(os.pathsep)
    env['C2_OWNER_FIXTURE_ACTION'] = 'controller'
    env['C2_OWNER_CHILD_PYTHON'] = sys.executable
    env['C2_OWNER_SHUTDOWN_MARKER'] = str(marker)
    env['C2_OWNER_GRACE_SECONDS'] = '0.3'
    env['C2_OWNER_SERVER_ID'] = f'owner-it-{os.getpid()}'
    env.update(extra)
    return env


class _ControllerProcess:
    """Drain the controller's stdout in one thread and expose markers."""

    def __init__(self, process: subprocess.Popen[str]) -> None:
        self.process = process
        self.lines: list[str] = []
        self._queue: queue.Queue[str | None] = queue.Queue()
        self._reader = threading.Thread(target=self._drain, daemon=True)
        self._reader.start()

    def _drain(self) -> None:
        stream = self.process.stdout
        if stream is None:
            self._queue.put(None)
            return
        for line in stream:
            self.lines.append(line)
            self._queue.put(line)
        self._queue.put(None)

    def read_until(self, marker: str, timeout: float = 60.0) -> str:
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(
                    f'timed out waiting for {marker!r}; last lines={self.lines[-8:]!r}',
                )
            try:
                line = self._queue.get(timeout=remaining)
            except queue.Empty:
                continue
            if line is None:
                raise AssertionError(
                    f'controller stdout ended before {marker!r}; '
                    f'stderr={self.stderr_text()!r}',
                )
            if marker in line:
                return line

    def stderr_text(self) -> str:
        if self.process.stderr is None:
            return ''
        try:
            return self.process.stderr.read()
        except Exception:  # pragma: no cover - best-effort diagnostics
            return ''

    def terminate(self) -> None:
        if self.process.poll() is None:
            self.process.kill()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:  # pragma: no cover - defensive
            pass
        if self.process.stdout is not None:
            self.process.stdout.close()
        if self.process.stderr is not None:
            self.process.stderr.close()

    def stdout_text(self) -> str:
        return '\n'.join(self.lines)


def _owner_launcher() -> Path:
    selected = os.environ.get('CTWO_TEST_OWNER_LAUNCHER')
    assert selected, 'Rust owner launcher must be prepared before timed tests'
    binary = Path(selected)
    assert binary.is_file(), binary
    return binary


def _prepare_owner_launcher() -> Path:
    """Compile once during collection, outside the business test deadline."""
    if os.environ.get('CTWO_TEST_OWNER_LAUNCHER'):
        return _owner_launcher()
    target = Path(os.environ.get('CARGO_TARGET_DIR', REPOSITORY_ROOT / 'sdk/rust/target'))
    environment = os.environ.copy()
    # The Rust SDK consumes the official CoreSDK; Python's extension may
    # independently have been built in source mode.
    if environment.get('FASTDB_PAYLOAD_SYSTEM_LIB_DIR'):
        environment['FASTDB_PAYLOAD_LINK_MODE'] = 'system'
    result = subprocess.run(
        ['cargo', 'build', '--locked', '--all-features', '--manifest-path', str(REPOSITORY_ROOT / 'sdk/rust/Cargo.toml'),
         '--example', 'owned_child', '--target-dir', str(target)],
        env=environment, capture_output=True, text=True, timeout=300)
    assert result.returncode == 0, result.stderr
    binary = target / 'debug/examples' / ('owned_child.exe' if os.name == 'nt' else 'owned_child')
    assert binary.is_file(), binary
    os.environ['CTWO_TEST_OWNER_LAUNCHER'] = str(binary)
    return binary


def _start_controller(tmp_path: Path, marker: Path, **extra: str) -> _ControllerProcess:
    process = subprocess.Popen(
        [str(_owner_launcher()), sys.executable, str(FIXTURE), 'service'],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        cwd=str(tmp_path),
        env=_subprocess_env(tmp_path, marker, **extra),
        **python_process_options(),
    )
    return _ControllerProcess(process)


def _business_call(
    tmp_path: Path,
    marker: Path,
    address: str,
    text: str,
) -> subprocess.CompletedProcess[str]:
    script = textwrap.dedent(
        f"""
        import sys
        sys.path.insert(0, {str(FIXTURE.parent)!r})
        import c_two as cc
        from owner_bound_service import OwnerBoundEcho

        proxy = cc.connect(OwnerBoundEcho, name='owned-echo', address={address!r})
        assert proxy.ping() == 'pong', proxy.ping()
        assert proxy.echo({text!r}) == {text!r}
        cc.close(proxy)
        cc.shutdown()
        print('BUSINESS_OK')
        """,
    )
    return subprocess.run(
        [sys.executable, '-c', script],
        capture_output=True,
        text=True,
        env=_subprocess_env(tmp_path, marker, C2_OWNER_FIXTURE_ACTION='client'),
        timeout=60,
    )


def _assert_no_capability_leak(text: str) -> None:
    for forbidden in ('/tmp/', 'keepalive', 'handle=', 'fd=', 'owner-control'):
        assert forbidden not in text, f'controller stdout leaked {forbidden!r}'


class TestOwnerBoundLifecycle:
    @pytest.mark.parametrize('release_mode', ['shutdown', 'drop'])
    def test_controller_release_stops_service_with_one_shutdown_hook(
        self,
        tmp_path: Path, release_mode: str,
    ) -> None:
        marker = tmp_path / 'shutdown_marker.txt'
        controller = _start_controller(tmp_path, marker, C2_OWNER_HOLD_SECONDS='3', C2_OWNER_RELEASE_MODE=release_mode)
        try:
            controller.read_until(SPAWNED_MARKER)
            ready = controller.read_until(READY_MARKER)
            address = ready.split(READY_MARKER, 1)[1].strip()
            assert address.startswith('ipc://'), address

            # While the controller still holds the keepalive, the resource
            # serves real business calls.
            probe = _business_call(tmp_path, marker, address, 'still-alive')
            assert probe.returncode == 0, probe.stderr
            assert 'BUSINESS_OK' in probe.stdout

            # The controller releases the capability: the service must reach a
            # real native terminal outcome, exit serve(), and run the hook once.
            controller.read_until(RELEASED_MARKER, timeout=30)
            finished = controller.read_until(FINISHED_MARKER, timeout=60)
            assert 'shutdown_calls=1' in finished
            assert controller.process.wait(timeout=30) == 0, controller.stderr_text()
            _assert_no_capability_leak(controller.stdout_text())
        finally:
            controller.terminate()

        assert marker.exists(), 'service never ran its shutdown hook'
        assert marker.read_text(encoding='utf-8') == '1'

    def test_controller_kill_stops_service_but_business_disconnects_do_not(
        self,
        tmp_path: Path,
    ) -> None:
        marker = tmp_path / 'kill_marker.txt'
        controller = _start_controller(tmp_path, marker, C2_OWNER_HOLD_SECONDS='120')
        try:
            controller.read_until(SPAWNED_MARKER)
            ready = controller.read_until(READY_MARKER)
            address = ready.split(READY_MARKER, 1)[1].strip()

            # Ordinary business disconnects never control the host's lifetime.
            for _ in range(2):
                probe = _business_call(tmp_path, marker, address, 'business')
                assert probe.returncode == 0, probe.stderr

            # The controller is still holding, so the service stays armed and
            # serving even though every business client has left.
            controller.read_until(HOLDING_MARKER, timeout=30)
            assert controller.process.poll() is None
            assert not marker.exists(), 'service stopped on a business disconnect'
            still_serving = _business_call(tmp_path, marker, address, 'after-idle')
            assert still_serving.returncode == 0, still_serving.stderr

            # SIGKILL the controller: the service observes only a control EOF.
            controller.process.kill()
            controller.process.wait(timeout=10)
            deadline = time.monotonic() + 45.0
            while time.monotonic() < deadline:
                if marker.exists() and marker.read_text(encoding='utf-8') == '1':
                    break
                time.sleep(0.1)
            assert marker.exists(), 'service never ran its shutdown hook after controller kill'
            assert marker.read_text(encoding='utf-8') == '1'
            finished = controller.read_until(FINISHED_MARKER, timeout=10)
            assert 'shutdown_calls=1' in finished
            controller._reader.join(timeout=5)
            assert not controller._reader.is_alive(), 'child did not exit and close inherited stdout'
            _assert_no_capability_leak(controller.stdout_text())
        finally:
            controller.terminate()


@pytest.mark.skipif(
    os.name == 'nt' and not hasattr(os, 'startfile'),
    reason='owned child spawn requires a supported platform',
)
class TestOwnerBoundNativeSurface:
    """The facade's native observation stays honest about a stopped host."""

    def test_native_observation_matches_in_process_termination(self, tmp_path: Path) -> None:
        from c_two.transport.registry import _ProcessRegistry

        marker = tmp_path / 'observation_marker.txt'
        controller_script = textwrap.dedent(
            """
            import time
            import c_two as cc

            @cc.crm(namespace='cc.test.owner_observation', version='0.1.0')
            class Echo:
                def ping(self) -> str:
                    ...

            class EchoResource:
                def ping(self) -> str:
                    return 'pong'

            keepalive, receiver = cc.owner_control_pair()
            cc.set_server(
                server_id='observation-controller',
                lifecycle=cc.LifecycleConfig.owner_bound(0.3),
                owner_control=receiver,
            )
            cc.register(Echo, EchoResource(), name='observed-echo')
            print('NATIVE_ARMED', cc.native_lifecycle_snapshot()['phase'], flush=True)
            keepalive.shutdown()
            deadline = time.monotonic() + 30
            while cc.native_terminal_outcome() is None:
                assert time.monotonic() < deadline
                time.sleep(0.05)
            snapshot = cc.native_lifecycle_snapshot()
            print('NATIVE_TERMINAL', snapshot['phase'], snapshot['terminal'], flush=True)
            cc.shutdown()
            print('NATIVE_DONE', flush=True)
            """,
        )
        env = _subprocess_env(tmp_path, marker, C2_OWNER_FIXTURE_ACTION='client')
        result = subprocess.run(
            [sys.executable, '-c', controller_script],
            capture_output=True,
            text=True,
            env=env,
            timeout=90,
        )
        assert result.returncode == 0, result.stderr
        assert 'NATIVE_ARMED armed' in result.stdout
        assert 'NATIVE_TERMINAL finished True' in result.stdout
        # This test process's own registry never saw that controller's host.
        assert _ProcessRegistry.get().native_lifecycle_snapshot() is None

@pytest.mark.parametrize('owner_bound', [False, True])
def test_short_shutdown_retains_inflight_borrowed_and_held_owners(owner_bound: bool) -> None:
    """The timeout limits the caller while the native drain retains real work."""
    import c_two as cc
    from concurrent.futures import ThreadPoolExecutor
    from fastdb4py.payload import Payload, PayloadError
    from tests.integration.test_portable_payload_runtime import SPEC, build_payload, read_value
    from c_two.transport.registry import _ProcessRegistry

    entered = threading.Event()
    finish = threading.Event()

    @cc.crm(namespace='test.owner-drain-payload', version='0.1.0')
    class PayloadService:
        @cc.transfer(input=SPEC, output=SPEC)
        def echo(self, payload: Payload) -> Payload:
            ...

        @cc.transfer(input=SPEC, output=SPEC)
        def blocked(self, payload: Payload) -> Payload:
            ...

        @cc.on_shutdown
        def cleanup(self) -> None:
            ...

    class Resource:
        borrowed = None
        hooks = 0

        def echo(self, payload: Payload) -> Payload:
            return payload

        def blocked(self, payload: Payload) -> Payload:
            self.borrowed = payload
            entered.set()
            assert finish.wait(10), 'test did not release the callback barrier'
            assert read_value(payload) == 7, 'native shutdown released borrowed input early'
            return payload

        def cleanup(self):
            self.hooks += 1

    cc.shutdown()
    keepalive = None
    if owner_bound:
        keepalive, receiver = cc.owner_control_pair()
        cc.set_server(lifecycle=cc.LifecycleConfig.owner_bound(0), owner_control=receiver)
    resource = Resource()
    source = build_payload()
    held = None
    proxy = None
    executor = ThreadPoolExecutor(max_workers=1)
    client_registry = _ProcessRegistry()
    serve_done = threading.Event()
    serve_thread = None
    try:
        cc.register(PayloadService, resource, name='blocked-payload',
                    input_lifetime={'blocked': cc.InputLifetime.BORROWED})
        registry = _ProcessRegistry.get()
        if owner_bound:
            def run_serve():
                cc.serve()
                serve_done.set()
            serve_thread = threading.Thread(target=run_serve, daemon=True)
            serve_thread.start()
            deadline = time.monotonic() + 5
            while registry._serve_stop is None:
                assert time.monotonic() < deadline
                time.sleep(0.01)
        else:
            cc.serve(blocking=False)
        session, server, address = registry._runtime_session, registry._server, cc.server_address()
        proxy = client_registry.connect(PayloadService, name='blocked-payload', address=address)
        held = cc.hold(proxy.echo)(source)
        retained = held.value
        assert read_value(retained) == 7
        future = executor.submit(proxy.blocked, source)
        assert entered.wait(5), 'real IPC callback did not enter'
        if owner_bound:
            keepalive.shutdown()
        started = time.monotonic()
        outcome = cc.shutdown(timeout=0.1)
        assert time.monotonic() - started < 0.8, outcome
        assert outcome['runtime_barrier_error'] or outcome['route_close_error'], outcome
        assert registry._runtime_session is session
        assert registry._server is server
        assert cc.server_address() == address
        assert server.names == ['blocked-payload']
        assert resource.hooks == 0
        if owner_bound:
            assert not serve_done.is_set(), 'serve exited while callback was draining'
        assert session.native_terminal_outcome() is None
        with pytest.raises(RuntimeError, match='still draining'):
            session.ensure_host_started()
        assert session.native_terminal_outcome() is None
        assert read_value(resource.borrowed) == 7
        assert read_value(retained) == 7
        finish.set()
        response = future.result(timeout=5)
        assert read_value(response) == 7
        response.close()
        with pytest.raises(PayloadError) as expired:
            resource.borrowed.entry_view(0)
        assert expired.value.symbol == 'VIEW_INVALIDATED'
        completed = cc.shutdown(timeout=5)
        assert not completed['runtime_barrier_error'] and not completed['route_close_error'], completed
        assert resource.hooks == 1
        assert server.names == []
        if owner_bound:
            assert serve_done.wait(5), 'serve never consumed the native terminal journal'
        assert not session.host_started
        assert session.shutdown(route_names=[], timeout_seconds=0) == completed
        assert read_value(retained) == 7
        held.release()
        with pytest.raises(PayloadError) as expired:
            retained.entry_view(0)
        assert expired.value.symbol == 'VIEW_INVALIDATED'
        cc.shutdown(timeout=0.1)
        assert resource.hooks == 1
    finally:
        finish.set()
        executor.shutdown(wait=True)
        if held is not None:
            held.release()
        source.close()
        if proxy is not None:
            cc.close(proxy)
        client_registry.shutdown()
        if serve_thread is not None:
            serve_thread.join(timeout=5)
            assert not serve_thread.is_alive()
        if keepalive is not None:
            keepalive.shutdown()
        cc.shutdown()
