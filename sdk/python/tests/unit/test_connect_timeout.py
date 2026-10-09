"""Thin connect-options projection, native validation, and local resource safety."""
from __future__ import annotations

import inspect
import threading
import time
from types import SimpleNamespace

import pytest

import c_two as cc
from c_two import _native
from c_two.crm.contract import crm_contract
from c_two.transport.registry import _ProcessRegistry


@cc.crm(namespace='test.connect_timeout', version='0.1.0')
class ConnectContract:
    def echo(self, value: object) -> object:
        ...


class Resource:
    calls = 0

    def echo(self, value):
        self.calls += 1
        return value


def test_public_connect_timeout_is_optional_keyword():
    timeout = inspect.signature(cc.connect).parameters['timeout']
    assert timeout.kind is inspect.Parameter.KEYWORD_ONLY
    assert timeout.default is None


@pytest.mark.parametrize('timeout', [-1, -0.01, float('nan'), float('inf'), float('-inf'), 1e300])
@pytest.mark.parametrize('mode', ['ipc', 'explicit_relay', 'relay'])
def test_native_connect_rejects_invalid_options_before_transport(timeout, mode):
    session = _native.RuntimeSession(use_process_relay_anchor=False)
    args = ('connect-timeout', *crm_contract(ConnectContract).native_args())
    with pytest.raises(ValueError, match='timeout'):
        if mode == 'ipc':
            session.acquire_ipc_client('ipc://unused', *args, timeout_seconds=timeout)
        elif mode == 'explicit_relay':
            session.connect_explicit_relay_http('http://127.0.0.1:9', *args, timeout_seconds=timeout)
        else:
            session.connect_via_relay(*args, timeout_seconds=timeout)
    assert not session.client_config_frozen


@pytest.mark.parametrize('address', ['ipc://unused', 'http://127.0.0.1:9', None])
def test_public_zero_timeout_has_connect_semantics_without_transport(address):
    _ProcessRegistry.reset()
    try:
        with pytest.raises(cc.error.CallDeadlineExceeded) as failure:
            cc.connect(ConnectContract, name='connect-timeout', address=address, timeout=0)
        assert failure.value.transport_phase == 'pre_dispatch'
        assert failure.value.details['operation'] == 'connect'
        assert failure.value.details['stage']
    finally:
        _ProcessRegistry.reset()


@pytest.mark.parametrize('timeout', [None, 0.25])
def test_local_connect_keeps_direct_objects_and_does_not_invoke_resource(timeout, monkeypatch):
    registry = _ProcessRegistry()
    resource = Resource()
    contract = crm_contract(ConnectContract)

    class LocalServer:
        def get_local_slot_info(self, name, *, connect_attempt=None):
            assert name == 'connect-timeout'
            assert connect_attempt is not None
            return resource, None, contract

    monkeypatch.setattr(registry, '_server', LocalServer())
    connection = registry.connect(ConnectContract, name='connect-timeout', timeout=timeout)
    assert resource.calls == 0
    value = object()
    assert connection.echo(value) is value
    assert resource.calls == 1
    registry.close(connection)


def test_native_local_zero_validation_runs_no_python_callback():
    with pytest.raises(_native.CoreError) as failure:
        _native.ConnectAttempt(timeout_seconds=0)
    assert failure.value.details['operation'] == 'connect'
    assert failure.value.details['stage'] == 'connect_start'


@pytest.mark.parametrize('timeout', [None, 0.125])
@pytest.mark.parametrize('address,method', [
    ('ipc://unused', 'acquire_ipc_client'),
    ('http://127.0.0.1:9', 'connect_explicit_relay_http'),
    (None, 'connect_via_relay'),
])
def test_registry_projects_budget_with_full_expected_contract(timeout, address, method, monkeypatch):
    registry = _ProcessRegistry()
    captured = []
    sync_attempts = []
    attempts = []
    native_attempt = _native.ConnectAttempt

    def begin(*, timeout_seconds=None):
        assert timeout_seconds == timeout
        attempt = native_attempt(timeout_seconds=timeout_seconds)
        attempts.append(attempt)
        return attempt

    monkeypatch.setattr(_native, 'ConnectAttempt', begin)

    class ProjectionReached(RuntimeError):
        pass

    class Session:
        def lease_tracker(self):
            return None

        def set_relay_anchor_address(self, address, *, connect_attempt=None):
            sync_attempts.append(connect_attempt)

        def __getattr__(self, name):
            assert name == method

            def projected(*args, **kwargs):
                captured.append((args, kwargs))
                raise ProjectionReached('native entry')
            return projected

    monkeypatch.setattr(registry, '_runtime_session', Session())
    expected_exception = cc.error.RegistryUnavailable if method == 'connect_via_relay' else ProjectionReached
    with pytest.raises(expected_exception):
        registry.connect(ConnectContract, name='connect-timeout', address=address, timeout=timeout)
    args = ('connect-timeout', *crm_contract(ConnectContract).native_args())
    if address is not None:
        args = (address, *args)
    assert len(attempts) == 1
    assert captured == [(args, {'connect_attempt': attempts[0]})]
    assert sync_attempts == ([attempts[0]] if method == 'connect_via_relay' else [])


def test_native_deadline_error_round_trip_preserves_connect_details():
    with pytest.raises(_native.CoreError) as failure:
        _native.ConnectAttempt(timeout_seconds=0)
    native = failure.value
    projected = cc.error.CCError.deserialize(native.error_bytes)
    assert isinstance(projected, cc.error.CallDeadlineExceeded)
    assert int(projected.code) == native.code == 715
    assert projected.message == native.message
    assert projected.details == dict(native.details)
    assert projected.details == {
        'operation': 'connect',
        'transport_phase': 'pre_dispatch',
        'stage': 'connect_start',
        'fallback_eligible': 'false',
        'route_withdrawal': 'false',
    }
    assert native.transport_phase == projected.transport_phase == 'pre_dispatch'
    assert native.fallback_eligible is False


@pytest.fixture
def local_registry(monkeypatch):
    """Exercise actual Python slot glue without binding a transport listener."""
    from c_two.transport.server.native import NativeServerBridge

    registry = _ProcessRegistry()
    resource = Resource()
    contract = crm_contract(ConnectContract)
    server = object.__new__(NativeServerBridge)
    server._slots_lock = threading.Lock()
    server._slots = {'connect-timeout': SimpleNamespace(
        direct_instance=resource, scheduler=None,
        crm_ns=contract.crm_ns, crm_name=contract.crm_name, crm_ver=contract.crm_ver,
        abi_hash=contract.abi_hash, signature_hash=contract.signature_hash,
    )}
    registry._server = server
    monkeypatch.setattr(_ProcessRegistry, '_instance', registry)
    monkeypatch.setattr(_ProcessRegistry, '_instance_lock', threading.Lock())
    return registry, server, resource


@pytest.mark.parametrize('target,stage', [
    ('singleton', 'registry_init_wait'),
    ('registry', 'registry_snapshot_wait'),
    ('local_slot', 'local_slot_wait'),
])
def test_connect_deadline_bounds_python_lock_before_owner_releases(target, stage, local_registry, monkeypatch):
    registry, server, resource = local_registry
    if target == 'singleton':
        monkeypatch.setattr(_ProcessRegistry, '_instance', None)
        lock = _ProcessRegistry._instance_lock
    else:
        lock = registry._lock if target == 'registry' else server._slots_lock
    held = threading.Event()
    released = threading.Event()

    def owner():
        with lock:
            held.set()
            time.sleep(0.3)
        released.set()

    worker = threading.Thread(target=owner)
    worker.start()
    assert held.wait(1)
    try:
        started = time.monotonic()
        with pytest.raises(cc.error.CallDeadlineExceeded) as failure:
            cc.connect(ConnectContract, name='connect-timeout', timeout=0.1)
        assert time.monotonic() - started < 0.25
        assert not released.is_set()
        assert lock.locked(), 'timeout must not release the lock still owned by another thread'
        assert failure.value.details['operation'] == 'connect'
        assert failure.value.details['transport_phase'] == 'pre_dispatch'
        assert failure.value.details['stage'] == stage
        assert resource.calls == 0
        assert not registry._runtime_session.client_config_frozen
        assert all(value == 0 for value in registry._runtime_session.path_counters().values())
    finally:
        worker.join(1)
        assert not worker.is_alive()
    assert not lock.locked()
    # Timeout released only this waiter's state; the same resource remains usable.
    if target == 'singleton':
        monkeypatch.setattr(_ProcessRegistry, '_instance', registry)
    connection = cc.connect(ConnectContract, name='connect-timeout', timeout=0.5)
    try:
        assert resource.calls == 0
        assert connection.echo('after') == 'after'
    finally:
        cc.close(connection)


def test_public_connect_carries_one_attempt_through_singleton_snapshot_and_local_lookup(local_registry, monkeypatch):
    registry, server, resource = local_registry
    real_attempt = _native.ConnectAttempt
    observed = []

    class RecordingAttempt:
        def __init__(self, *, timeout_seconds=None):
            self.native = real_attempt(timeout_seconds=timeout_seconds)
            observed.append(('begin', timeout_seconds, self))

        def check(self, stage):
            observed.append(('check', stage, self))
            self.native.check(stage)

        def acquire_lock(self, lock, stage):
            observed.append(('lock', stage, self))
            self.native.acquire_lock(lock, stage)

    monkeypatch.setattr(_native, 'ConnectAttempt', RecordingAttempt)
    connection = cc.connect(ConnectContract, name='connect-timeout', timeout=0.5)
    try:
        assert resource.calls == 0
        assert len([entry for entry in observed if entry[0] == 'begin']) == 1
        attempt = observed[0][2]
        assert all(entry[2] is attempt for entry in observed)
        assert ('check', 'registry_init', attempt) in observed
        assert ('lock', 'registry_snapshot_wait', attempt) in observed
        assert ('lock', 'local_slot_wait', attempt) in observed
        assert ('check', 'python_connect_finish', attempt) in observed
    finally:
        cc.close(connection)


def test_connect_snapshots_session_once_for_relay_projection(monkeypatch):
    registry = _ProcessRegistry()
    observed = []

    class ReplacedSession:
        def __getattr__(self, name):
            raise AssertionError(f'connect reread replaced session: {name}')

    class CapturedSession:
        def lease_tracker(self):
            registry._runtime_session = ReplacedSession()
            return None

        def set_relay_anchor_address(self, address, *, connect_attempt=None):
            observed.append(connect_attempt)

        def connect_via_relay(self, *args, connect_attempt=None):
            observed.append(connect_attempt)
            raise _native.CoreError('projection reached')

    registry._runtime_session = CapturedSession()
    with pytest.raises(cc.error.RegistryUnavailable, match='projection reached'):
        registry.connect(ConnectContract, name='connect-timeout', timeout=0.5)
    assert len(observed) == 2
    assert observed[0] is observed[1]


def test_zero_budget_expires_before_singleton_lookup(monkeypatch):
    def must_not_lookup(cls, *, connect_attempt=None):
        raise AssertionError('zero deadline reached singleton lookup')

    monkeypatch.setattr(_ProcessRegistry, 'get', classmethod(must_not_lookup))
    with pytest.raises(cc.error.CallDeadlineExceeded) as failure:
        cc.connect(ConnectContract, name='connect-timeout', timeout=0)
    assert failure.value.details['stage'] == 'connect_start'


def test_python_preparation_and_native_entry_keep_original_attempt_budget(monkeypatch):
    registry = _ProcessRegistry()
    native_session = registry._runtime_session
    observed = []

    class Session:
        def lease_tracker(self):
            time.sleep(0.06)
            return native_session.lease_tracker()

        def acquire_ipc_client(self, *args, connect_attempt=None):
            observed.append(connect_attempt)
            time.sleep(0.06)
            return native_session.acquire_ipc_client(*args, connect_attempt=connect_attempt)

    registry._runtime_session = Session()
    # The malformed address would fail validation if Core starts a new budget.
    # The expired shared attempt must win before any OS connection is attempted.
    with pytest.raises(cc.error.CallDeadlineExceeded) as failure:
        registry.connect(ConnectContract, name='connect-timeout', address='malformed', timeout=0.1)
    assert len(observed) == 1
    assert failure.value.details['stage'] == 'connect_start'
    assert failure.value.details['operation'] == 'connect'
    assert not native_session.client_config_frozen
    assert all(value == 0 for value in native_session.path_counters().values())


def test_native_unlimited_attempt_keeps_python_lock_behavior():
    lock = threading.Lock()
    attempt = _native.ConnectAttempt(timeout_seconds=None)
    attempt.acquire_lock(lock, 'registry_snapshot_wait')
    try:
        assert lock.locked()
        attempt.check('python_connect_finish')
    finally:
        lock.release()



def test_expiry_after_python_lock_acquisition_releases_only_new_acquisition():
    lock = threading.Lock()
    released = []

    class DelayedAcquire:
        def acquire(self, *args, **kwargs):
            acquired = lock.acquire(*args, **kwargs)
            time.sleep(0.06)
            return acquired

        def release(self):
            released.append(True)
            lock.release()

    attempt = _native.ConnectAttempt(timeout_seconds=0.05)
    with pytest.raises(_native.CoreError) as failure:
        attempt.acquire_lock(DelayedAcquire(), 'registry_snapshot_wait')
    assert failure.value.details['stage'] == 'registry_snapshot_wait'
    assert released == [True]
    assert not lock.locked()



def test_relay_override_deadline_keeps_canonical_error_before_registry_wrapping():
    registry = _ProcessRegistry()

    class Session:
        def lease_tracker(self):
            return None

        def set_relay_anchor_address(self, address, *, connect_attempt=None):
            time.sleep(0.06)
            connect_attempt.check('relay_override_wait')

        def connect_via_relay(self, *args, **kwargs):
            raise AssertionError('expired override reached relay discovery')

    registry._runtime_session = Session()
    with pytest.raises(cc.error.CallDeadlineExceeded) as failure:
        registry.connect(ConnectContract, name='connect-timeout', timeout=0.05)
    assert failure.value.details['operation'] == 'connect'
    assert failure.value.details['transport_phase'] == 'pre_dispatch'
    assert failure.value.details['stage'] == 'relay_override_wait'
