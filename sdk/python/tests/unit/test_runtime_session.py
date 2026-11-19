from __future__ import annotations

import logging
from pathlib import Path

import pytest

from tests.fixtures import local_server

from c_two._native import RuntimeSession


BAD_SHUTDOWN_TIMEOUTS = [float('inf'), float('-inf'), float('nan'), -1.0, -0.001, 1e300, float(2**64)]


@pytest.mark.parametrize('timeout', BAD_SHUTDOWN_TIMEOUTS)
def test_native_shutdown_rejects_invalid_timeout_without_changing_identity(timeout: float) -> None:
    from c_two._native import RuntimeSession

    session = RuntimeSession(server_id='timeout-validation')
    with pytest.raises(ValueError, match='timeout_seconds'):
        session.shutdown(timeout_seconds=timeout)
    assert session.server_id is None
    assert session.server_address is None
    # A rejected call leaves the session usable for an ordinary shutdown.
    outcome = dict(session.shutdown(timeout_seconds=0.0))
    assert outcome['completed'] is True
    assert outcome['ipc_clients_drained'] is False
    assert outcome['runtime_barrier_error'] is None
    assert not session.retired
    session.check_active()
    session.clear_server_identity()


@pytest.mark.parametrize('timeout', [0.0, -0.0, 0.125, 5.0])
def test_native_shutdown_accepts_zero_and_normal_timeout(timeout: float) -> None:
    from c_two._native import RuntimeSession

    session = RuntimeSession()
    before = session.call_execution_snapshot()
    assert before['client']['closed'] is before['server']['closed'] is False
    outcome = dict(session.shutdown(timeout_seconds=timeout))
    assert outcome['completed'] is True
    assert outcome['ipc_clients_drained'] is False
    assert outcome['runtime_barrier_error'] is None
    assert not session.retired
    session.check_active()
    session.clear_server_identity()
    assert session.call_execution_snapshot() == before


def test_invalid_native_shutdown_preserves_registered_host_and_shutdown_hook() -> None:
    import c_two as cc
    import c_two.endpoint
    from c_two.transport.registry import _get_default_runtime

    @cc.crm(namespace='test.shutdown-timeout-boundary', version='0.1.0')
    class Alive:
        def ping(self) -> str:
            ...

        @cc.on_shutdown
        def stop(self) -> None:
            ...

    class AliveResource:
        shutdown_calls = 0

        def ping(self) -> str:
            return 'pong'

        def stop(self) -> None:
            self.shutdown_calls += 1

    cc.shutdown()
    resource = AliveResource()
    assert cc.endpoint.server_credential() is None
    proxy = None
    try:
        cc.register(Alive, resource, name='timeout-alive')
        session = _get_default_runtime()._runtime_session  # noqa: SLF001
        identity = (session.server_id, session.server_address)
        credential = cc.endpoint.server_credential()
        assert credential.address == identity[1]
        assert credential.to_json() == session.server_credential.to_json()
        proxy = cc.connect(Alive, name='timeout-alive', address=cc.server_address())
        assert proxy.ping() == 'pong'
        for timeout in BAD_SHUTDOWN_TIMEOUTS:
            with pytest.raises(ValueError, match='timeout_seconds'):
                session.shutdown(timeout_seconds=timeout)
            assert (session.server_id, session.server_address) == identity
            assert cc.endpoint.server_credential().to_json() == credential.to_json()
            assert proxy.ping() == 'pong'
            assert resource.shutdown_calls == 0
        cc.shutdown()
        assert cc.endpoint.server_credential() is None
        assert resource.shutdown_calls == 1
        cc.shutdown()
        assert resource.shutdown_calls == 1
    finally:
        if proxy is not None:
            cc.close(proxy)
        cc.shutdown()


def test_native_runtime_session_explicit_identity_is_lazy() -> None:
    from c_two._native import RuntimeSession

    session = RuntimeSession(server_id='unit-server')
    assert session.server_id is None
    assert session.server_address is None

    identity = session.ensure_server()
    assert identity['server_id'] == 'unit-server'
    assert isinstance(identity['server_instance_id'], str)
    assert identity['server_instance_id']
    assert identity['ipc_address'] == 'ipc://unit-server'
    assert session.server_id == 'unit-server'
    assert session.server_address == 'ipc://unit-server'
    assert session.server_credential is None  # Allocating an address did not bind.


def test_native_runtime_session_rejects_invalid_server_ids() -> None:
    from c_two._native import RuntimeSession

    for bad in ['', ' ', 'bad/name', 'bad\\name', '.', '..', 'bad\nid']:
        with pytest.raises(ValueError, match='server_id'):
            RuntimeSession(server_id=bad)


def test_registry_identity_is_native_owned() -> None:
    import inspect
    from c_two.transport import registry

    source = inspect.getsource(registry)
    assert 'import uuid' not in source
    assert 'import os' not in source
    assert '_auto_server_id' not in source
    assert '_address_for_server_id' not in source


def test_native_host_start_validates_configuration_without_losing_bridge(monkeypatch) -> None:
    session = RuntimeSession(server_id='bridge-owner', use_process_relay_anchor=False)
    server = session.ensure_server_bridge()
    for key in ('C2_MAX_PAYLOAD_SIZE', 'C2_MAX_EXECUTION_WORKERS'):
        monkeypatch.setenv(key, 'invalid')
        with pytest.raises(RuntimeError, match=key):
            server.start()
        assert not session.host_started
        monkeypatch.delenv(key)
    try:
        assert session.ensure_server_bridge() is server
        server.start()
        assert session.host_started
        assert session.server_id == 'bridge-owner'
    finally:
        assert server.shutdown()['completed']
    assert not session.host_started


def test_failed_server_construction_does_not_publish_identity() -> None:
    import c_two as cc
    import c_two.endpoint

    @cc.crm(namespace='cc.test.runtime_session', version='0.1.0')
    class BrokenConfigCRM:
        def ping(self) -> str:
            ...

    class BrokenConfigImpl:
        def ping(self) -> str:
            return 'pong'

    cc.shutdown()
    try:
        with pytest.raises(ValueError):
            cc.set_server(max_payload_size=0)
        assert cc.endpoint.server_id() is None
        assert cc.server_address() is None
        cc.register(BrokenConfigCRM, BrokenConfigImpl(), name='broken-config')
    finally:
        cc.shutdown()


def test_explicit_ipc_connect_ignores_bad_relay_after_native_identity() -> None:
    import c_two as cc
    import c_two.endpoint
    from c_two.transport.registry import _reset_default_runtime

    @cc.crm(namespace='cc.test.runtime_session_bad_relay', version='0.1.0')
    class BadRelayCRM:
        def ping(self) -> str:
            ...

    class BadRelayImpl:
        def ping(self) -> str:
            return 'pong'

    _reset_default_runtime()
    try:
        cc.register(BadRelayCRM, BadRelayImpl(), name='bad-relay-route')
        address = cc.server_address()
        assert address is not None

        client = cc.Runtime()
        client.set_relay_anchor('http://127.0.0.1:9')
        try:
            with client.connect(BadRelayCRM, name='bad-relay-route', address=address) as crm:
                assert crm.ping() == 'pong'
        finally:
            client.shutdown()
    finally:
        _reset_default_runtime()


def test_name_only_connect_without_relay_ignores_bad_relay_proxy_env(monkeypatch) -> None:
    import c_two as cc
    import c_two.endpoint
    from c_two.transport.registry import _reset_default_runtime

    @cc.crm(namespace='cc.test.runtime_session_no_relay_bad_proxy', version='0.1.0')
    class NoRelayBadProxyCRM:
        def ping(self) -> str:
            ...

    _reset_default_runtime()
    monkeypatch.setenv('C2_ENV_FILE', '')
    monkeypatch.setenv('C2_RELAY_USE_PROXY', 'not-a-bool')
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    try:
        with pytest.raises(LookupError, match='not registered locally'):
            cc.connect(NoRelayBadProxyCRM, name='missing-no-relay')
    finally:
        _reset_default_runtime()


def test_no_relay_register_unregister_ignores_bad_relay_proxy_env(monkeypatch) -> None:
    import c_two as cc
    import c_two.endpoint
    from c_two.transport.registry import _reset_default_runtime

    @cc.crm(namespace='cc.test.runtime_session_bad_proxy_env', version='0.1.0')
    class BadProxyEnvCRM:
        def ping(self) -> str:
            ...

    class BadProxyEnvImpl:
        def ping(self) -> str:
            return 'pong'

    _reset_default_runtime()
    monkeypatch.setenv('C2_ENV_FILE', '')
    monkeypatch.setenv('C2_RELAY_USE_PROXY', 'not-a-bool')
    try:
        cc.register(BadProxyEnvCRM, BadProxyEnvImpl(), name='bad-proxy-env')
        address = cc.server_address()
        assert address is not None

        crm = cc.connect(BadProxyEnvCRM, name='bad-proxy-env', address=address)
        try:
            assert crm.ping() == 'pong'
        finally:
            cc.close(crm)

        cc.unregister('bad-proxy-env')
        cc.shutdown()
    finally:
        _reset_default_runtime()


def test_shutdown_ignores_late_bad_relay_env_file_and_closes_server(
    monkeypatch,
    tmp_path,
) -> None:
    import c_two as cc
    import c_two.endpoint
    from c_two.transport.registry import _reset_default_runtime

    @cc.crm(namespace='cc.test.runtime_session_bad_shutdown_env', version='0.1.0')
    class BadShutdownEnvCRM:
        def ping(self) -> str:
            ...

    class BadShutdownEnvImpl:
        def ping(self) -> str:
            return 'pong'

    _reset_default_runtime()
    bad_env_dir = tmp_path / 'env-dir'
    bad_env_dir.mkdir()
    try:
        cc.register(BadShutdownEnvCRM, BadShutdownEnvImpl(), name='bad-shutdown-env')
        assert cc.server_address() is not None

        monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
        monkeypatch.setenv('C2_ENV_FILE', str(bad_env_dir))
        cc.shutdown()

        assert cc.server_address() is None
    finally:
        _reset_default_runtime()


def test_shutdown_ignores_late_bad_relay_proxy_env_and_closes_server(
    monkeypatch,
) -> None:
    import c_two as cc
    import c_two.endpoint
    from c_two.transport.registry import _reset_default_runtime

    @cc.crm(namespace='cc.test.runtime_session_bad_shutdown_proxy', version='0.1.0')
    class BadShutdownProxyCRM:
        def ping(self) -> str:
            ...

    class BadShutdownProxyImpl:
        def ping(self) -> str:
            return 'pong'

    _reset_default_runtime()
    try:
        monkeypatch.setenv('C2_ENV_FILE', '')
        monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
        cc.register(BadShutdownProxyCRM, BadShutdownProxyImpl(), name='bad-shutdown-proxy')
        assert cc.server_address() is not None

        monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', 'http://127.0.0.1:9')
        monkeypatch.setenv('C2_RELAY_USE_PROXY', 'not-a-bool')
        cc.shutdown()

        assert cc.server_address() is None
    finally:
        _reset_default_runtime()


def test_failed_first_route_registration_clears_native_identity(monkeypatch) -> None:
    import c_two as cc
    import c_two.endpoint
    from c_two.transport.server.native import NativeServerBridge

    @cc.crm(namespace='cc.test.runtime_session_register_fail', version='0.1.0')
    class FailingRegisterCRM:
        def ping(self) -> str:
            ...

    class FailingRegisterImpl:
        def ping(self) -> str:
            return 'pong'

    def fail_register(self, *args, **kwargs):  # noqa: ANN001, ARG001
        raise RuntimeError('synthetic register failure')

    cc.shutdown()
    monkeypatch.setattr(NativeServerBridge, 'register_crm', fail_register)
    try:
        with pytest.raises(RuntimeError, match='synthetic register failure'):
            cc.register(FailingRegisterCRM, FailingRegisterImpl(), name='fail-register')
        assert cc.endpoint.server_id() is None
        assert cc.server_address() is None
    finally:
        cc.shutdown()


def test_failed_server_bridge_construction_clears_native_identity(monkeypatch) -> None:
    import c_two as cc
    import c_two.endpoint
    from c_two.transport.server import native as server_native

    @cc.crm(namespace='cc.test.runtime_session_bridge_fail', version='0.1.0')
    class FailingBridgeCRM:
        def ping(self) -> str:
            ...

    class FailingBridgeImpl:
        def ping(self) -> str:
            return 'pong'

    class FailingBridge:
        def __init__(self, *args, **kwargs):  # noqa: ANN001, ARG002
            raise RuntimeError('synthetic bridge construction failure')

    cc.shutdown()
    monkeypatch.setattr(server_native, 'NativeServerBridge', FailingBridge)
    try:
        with pytest.raises(RuntimeError, match='synthetic bridge construction failure'):
            cc.register(FailingBridgeCRM, FailingBridgeImpl(), name='fail-bridge')
        assert cc.endpoint.server_id() is None
        assert cc.server_address() is None
    finally:
        cc.shutdown()


def test_registry_does_not_construct_server_directly() -> None:
    import inspect
    from c_two.transport import registry

    source = inspect.getsource(registry)
    assert 'self._server = Server(' not in source
    assert 'server = Server(' not in source


def test_core_error_projection_exposes_structured_registration_failure_attrs() -> None:
    repo_root = Path(__file__).resolve().parents[4]
    source = (
        repo_root / 'sdk/python/native/src/core_error_ffi.rs'
    ).read_text(encoding='utf-8')

    assert 'LifecycleError::RegisterFailure(failure)' in source
    assert 'registration_failure' in source
    assert 'set_attr(value, "rollback"' in source
    assert 'set_attr(value, "relay_cleanup_error"' in source
    assert 'register_failure_to_dict' in source
    assert 'relay_cleanup_error_to_dict' in source
    assert 'route_close_outcome_to_dict(py, rollback)' in source
    assert 'registration_rollback_to_dict' not in source


def test_registry_does_not_own_direct_ipc_client_pool() -> None:
    import inspect
    from c_two.transport import registry

    source = inspect.getsource(registry)
    assert 'RustClientPool.instance()' not in source
    assert 'self._pool =' not in source
    assert 'self._pool.' not in source
    assert '_ensure_pool_config' not in source
    assert '_pool_config_applied' not in source


def test_phase3_removes_python_registration_rollback_authority() -> None:
    import inspect
    from c_two.transport import registry

    source = inspect.getsource(registry)
    assert '_rollback_registration' not in source


def test_failed_native_registration_leaves_existing_route_usable(monkeypatch) -> None:
    import c_two as cc
    import c_two.endpoint
    from c_two.transport.server.native import NativeServerBridge

    @cc.crm(namespace='cc.test.runtime_session_phase3', version='0.1.0')
    class Phase3CRM:
        def ping(self) -> str:
            ...

    class Phase3Impl:
        def __init__(self, value: str) -> None:
            self.value = value

        def ping(self) -> str:
            return self.value

    original_register = NativeServerBridge.register_crm

    def fail_bad_route(self, crm_class, crm_instance, concurrency=None, *, name=None, **kwargs):  # noqa: ANN001
        if name == 'bad':
            raise RuntimeError('synthetic native register failure')
        return original_register(
            self,
            crm_class,
            crm_instance,
            concurrency,
            name=name,
            **kwargs,
        )

    cc.shutdown()
    monkeypatch.setattr(NativeServerBridge, 'register_crm', fail_bad_route)
    try:
        cc.register(Phase3CRM, Phase3Impl('good'), name='good')
        with pytest.raises(RuntimeError, match='synthetic native register failure'):
            cc.register(Phase3CRM, Phase3Impl('bad'), name='bad')

        assert cc.server_address() is not None
        from c_two.transport.registry import _get_default_runtime
        registry = _get_default_runtime()
        assert registry.names == ['good']
        crm = registry.connect(Phase3CRM, name='good')
        try:
            assert crm.ping() == 'good'
        finally:
            cc.close(crm)
    finally:
        cc.shutdown()


def test_failed_native_registration_leaves_no_route_or_identity(monkeypatch) -> None:
    import c_two as cc
    import c_two.endpoint
    from c_two.transport.server.native import NativeServerBridge
    from c_two.transport.registry import _get_default_runtime

    @cc.crm(namespace='cc.test.runtime_session_phase3_first', version='0.1.0')
    class Phase3FirstCRM:
        def ping(self) -> str:
            ...

    class Phase3FirstImpl:
        def ping(self) -> str:
            return 'pong'

    def fail_register(*_args, **_kwargs):
        raise RuntimeError('synthetic native register failure')

    cc.shutdown()
    monkeypatch.setattr(NativeServerBridge, 'register_crm', fail_register)
    try:
        with pytest.raises(RuntimeError, match='synthetic native register failure'):
            cc.register(Phase3FirstCRM, Phase3FirstImpl(), name='first')

        registry = _get_default_runtime()
        assert registry.names == []
        assert registry.server_address() is None
        assert registry.server_id() is None
        with pytest.raises(LookupError, match='not registered locally'):
            registry.connect(Phase3FirstCRM, name='first')
    finally:
        cc.shutdown()


def test_direct_ipc_acquire_waits_for_dynamic_route_publication() -> None:
    import threading
    import time

    import c_two as cc

    import c_two.endpoint
    from c_two.transport.registry import Runtime

    @cc.crm(namespace='cc.test.runtime_session_dynamic_route', version='0.1.0')
    class DynamicRouteCRM:
        def ping(self) -> str:
            ...

    class DynamicRouteImpl:
        def ping(self) -> str:
            return 'pong'

    registrar = Runtime()
    resolver = Runtime()
    crm = None

    try:
        server = registrar._runtime_session.ensure_server_bridge()  # noqa: SLF001
        server.start()
        address = registrar.server_address()
        assert address is not None

        def delayed_register() -> None:
            time.sleep(0.12)
            registrar.register(
                DynamicRouteCRM,
                DynamicRouteImpl(),
                name='dynamic-route',
            )

        thread = threading.Thread(target=delayed_register)
        thread.start()
        try:
            crm = resolver.connect(
                DynamicRouteCRM,
                name='dynamic-route',
                address=address,
            )
            assert crm.ping() == 'pong'
        finally:
            thread.join(timeout=2)
            assert not thread.is_alive()
    finally:
        if crm is not None:
            cc.close(crm)
        resolver.shutdown()
        registrar.shutdown()


def test_set_client_preserves_pending_server_id_override() -> None:
    import c_two as cc
    import c_two.endpoint

    @cc.crm(namespace='cc.test.runtime_session_client_preserve', version='0.1.0')
    class PreserveCRM:
        def ping(self) -> str:
            ...

    class PreserveImpl:
        def ping(self) -> str:
            return 'pong'

    cc.shutdown()
    try:
        cc.set_server(server_id='unit-preserve-server')
        cc.set_client(memory_budget_bytes=16 * 1024 * 1024)
        cc.register(PreserveCRM, PreserveImpl(), name='preserve-route')
        assert cc.endpoint.server_id() == 'unit-preserve-server'
        assert cc.server_address() == 'ipc://unit-preserve-server'
    finally:
        cc.shutdown()


def test_set_client_after_direct_ipc_connect_raises() -> None:
    import pytest
    import c_two as cc
    import c_two.endpoint

    @cc.crm(namespace='cc.test.runtime_session_client_freeze', version='0.1.0')
    class FreezeCRM:
        def ping(self) -> str:
            ...

    class FreezeImpl:
        def ping(self) -> str:
            return 'pong'

    cc.shutdown()
    try:
        cc.register(FreezeCRM, FreezeImpl(), name='freeze-route')
        address = cc.server_address()
        crm = cc.connect(FreezeCRM, name='freeze-route', address=address)
        try:
            with pytest.raises(cc.error.CoreError, match='configuration is frozen'):
                cc.set_client(memory_budget_bytes=19 * 1024 * 1024)
        finally:
            cc.close(crm)
    finally:
        cc.shutdown()


def test_set_client_after_register_preserves_server_identity() -> None:
    import c_two as cc
    import c_two.endpoint

    @cc.crm(namespace='cc.test.runtime_session_client_after_register', version='0.1.0')
    class AfterRegisterCRM:
        def ping(self) -> str:
            ...

    class AfterRegisterImpl:
        def ping(self) -> str:
            return 'pong'

    cc.shutdown()
    try:
        cc.set_server(server_id='unit-after-register')
        cc.register(AfterRegisterCRM, AfterRegisterImpl(), name='after-register-route')
        cc.set_client(memory_budget_bytes=16 * 1024 * 1024)
        assert cc.endpoint.server_id() == 'unit-after-register'
        assert cc.server_address() == 'ipc://unit-after-register'
        crm = cc.connect(
            AfterRegisterCRM,
            name='after-register-route',
            address=cc.server_address(),
        )
        try:
            assert crm.ping() == 'pong'
        finally:
            cc.close(crm)
    finally:
        cc.shutdown()


def test_set_server_after_client_only_connect_preserves_client_freeze() -> None:
    import pytest
    import c_two as cc
    import c_two.endpoint
    from c_two.transport import Server

    @cc.crm(namespace='cc.test.runtime_session_server_after_client', version='0.1.0')
    class ServerAfterClientCRM:
        def ping(self) -> str:
            ...

    class ServerAfterClientImpl:
        def ping(self) -> str:
            return 'pong'

    address = 'ipc://unit_server_after_client'
    server = local_server(address)
    server.register_crm(
        ServerAfterClientCRM,
        ServerAfterClientImpl(),
        name='server-after-client',
    )
    server.start()
    cc.shutdown()
    try:
        crm = cc.connect(ServerAfterClientCRM, name='server-after-client', address=address)
        try:
            cc.set_server(server_id='unit-later-server')
            with pytest.raises(cc.error.CoreError, match='configuration is frozen'):
                cc.set_client(memory_budget_bytes=19 * 1024 * 1024)
        finally:
            cc.close(crm)
    finally:
        server.shutdown()
        cc.shutdown()


def test_runtime_session_exposes_no_language_owned_http_pool(monkeypatch) -> None:
    from c_two._native import RuntimeSession

    monkeypatch.setenv('C2_ENV_FILE', '')
    monkeypatch.setenv('C2_RELAY_USE_PROXY', 'false')
    session = RuntimeSession()

    assert not hasattr(session, 'acquire_http_client')
    assert not hasattr(session, 'release_http_client')
    assert not hasattr(session, 'http_client_refcount')

    outcome = dict(session.shutdown())

    assert outcome['route_outcomes'] == []
    assert outcome['route_close_error'] is None
    assert outcome['runtime_barrier_error'] is None


def test_registry_projects_native_semantic_before_wrapping() -> None:
    from c_two.error import CoreError, FallbackDenied
    from c_two.transport.registry import _cc_error_from_native_exception

    native_exc = CoreError('semantic', 'native failure')
    native_exc.code = 714
    native_exc.name = 'FallbackDenied'
    native_exc.details = {'route': 'grid'}
    native_exc.semantic_message = 'same local relay HTTP fallback denied'
    native_exc._transport_phase = 'pre_dispatch'
    native_exc._semantic_snapshot = (
        native_exc.code, native_exc.semantic_message, tuple(native_exc.details.items()),
        native_exc.transport_phase,
    )

    restored = _cc_error_from_native_exception(native_exc)

    assert isinstance(restored, FallbackDenied)
    assert restored.message == 'same local relay HTTP fallback denied'
    assert restored.details == {'route': 'grid'}
    assert restored.transport_phase == 'pre_dispatch'
    # Failures without a canonical semantic payload pass through untouched.
    assert _cc_error_from_native_exception(CoreError('transport', 'io')) is None
    assert _cc_error_from_native_exception(RuntimeError('plain')) is None

def test_shutdown_warns_when_native_cleanup_barrier_is_unconfirmed(caplog) -> None:
    from c_two._native import LocalEndpointContext, resolve_local_endpoint_context
    from c_two.transport.registry import Runtime

    class FakeRetiredObservation:
        """Retirement bundle double: an opaque handoff token."""

    retired_observations: list[FakeRetiredObservation] = []

    class FakeSession:
        def __init__(
            self,
            outcome: dict | None = None,
            *,
            max_outstanding_calls: int | None = None,
            memory_budget_bytes: int | None = None,
            **_kwargs,
        ) -> None:
            self.relay_anchor_address_override = None
            self._outcome = outcome if outcome is not None else {'relay_errors': []}
            self.adopted_observations: list[object] = []
            self.begin_shutdown_timeouts: list[float] = []
            self._context = resolve_local_endpoint_context()
            self._limits = {
                key: value for key, value in (
                    ('max_outstanding_calls', max_outstanding_calls),
                    ('memory_budget_bytes', memory_budget_bytes),
                ) if value is not None
            }

        @property
        def call_execution_limits_overrides(self) -> dict[str, int]:
            return dict(self._limits)

        def local_endpoint_context(self) -> LocalEndpointContext:
            return self._context

        def inherit_local_endpoint_selection(self, previous: FakeSession) -> None:
            self._context = previous._context

        def set_relay_anchor_address(self, address: str | None) -> None:
            self.relay_anchor_address_override = address

        retired = False

        def check_active(self, connect_attempt=None) -> None:  # noqa: ARG002
            return None

        def begin_shutdown(self, timeout_seconds: float = 5.0):
            self.begin_shutdown_timeouts.append(timeout_seconds)
            return self

        def observe(self):
            return {**self._outcome, 'completed': not any(self._outcome.get(key) for key in (
                'ipc_client_close_error', 'runtime_barrier_error'))}

        def retire_memory_observation(self) -> FakeRetiredObservation:
            observation = FakeRetiredObservation()
            retired_observations.append(observation)
            return observation

        def inherit_retirement(self, previous) -> None:
            self._limits = previous._limits
            self.adopted_observations.append(previous.retire_memory_observation())

    def hostless_registry(outcome: dict) -> Runtime:
        registry = Runtime()
        registry._runtime_session = FakeSession(outcome)  # noqa: SLF001
        registry._server = None  # noqa: SLF001
        return registry

    logger_name = 'c_two.transport.registry'
    with caplog.at_level(logging.WARNING, logger=logger_name):
        pending_ipc = hostless_registry({
            'relay_errors': [],
            'ipc_client_close_error': (
                'unconfirmed IPC client cache closes for ["ipc://blocked"]'
            ),
        })
        ipc_session = pending_ipc._runtime_session
        assert not pending_ipc.shutdown()["completed"]
        assert pending_ipc._runtime_session is ipc_session
        # Runtime.shutdown's default budget is forwarded to the native
        # begin_shutdown transaction, and a pending outcome never begins a
        # second one.
        assert ipc_session.begin_shutdown_timeouts == [30.0]
        pending_runtime = hostless_registry({
            'relay_errors': [],
            'runtime_barrier_error': 'runtime barrier did not quiesce',
        })
        runtime_session = pending_runtime._runtime_session
        assert not pending_runtime.shutdown()["completed"]
        assert pending_runtime._runtime_session is runtime_session
        assert runtime_session.begin_shutdown_timeouts == [30.0]
        # Absent keys stay silent: apparent full cleanup is only reported
        # when the native outcome actually reports a failure.
        clean = hostless_registry({'relay_errors': []})
        clean_session = clean._runtime_session
        clean.shutdown()
        assert clean._runtime_session is not clean_session
        assert clean_session.begin_shutdown_timeouts == [30.0]
        assert (
            clean._runtime_session.local_endpoint_context()
            is clean_session.local_endpoint_context()
        )
        assert clean._runtime_session.adopted_observations == retired_observations

    messages = [record.getMessage() for record in caplog.records]
    ipc_warnings = [
        message for message in messages if 'IPC client cache' in message
    ]
    assert ipc_warnings, 'an unconfirmed native IPC client close must be surfaced'
    assert any('ipc://blocked' in message for message in ipc_warnings)
    assert any(
        'runtime barrier' in message for message in messages
    ), 'an unconfirmed runtime barrier must be surfaced'
    assert len(messages) == 2, f'clean outcomes must stay silent, saw {messages!r}'

    # Barrier confirmation is transport cleanup reporting only: it never
    # touches the retirement observation. Pending sessions stay owned; only
    # completed cleanup retires once, and the observation exposes no fence
    # the registry could mark — its lifetime is decided by real owners.
    assert len(retired_observations) == 1
    for observation in retired_observations:
        surface = {name for name in dir(observation) if not name.startswith('_')}
        assert not hasattr(observation, 'mark_close_confirmed')
        assert not hasattr(observation, 'close_confirmed')
        assert not surface & {
            'track', 'track_retained', 'reserve', 'prune', 'scope_reports',
            'lease_stats',
        }, f'observation must stay an opaque handoff, saw {surface!r}'
