from __future__ import annotations

import logging
from pathlib import Path

import pytest


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


def test_native_runtime_session_projects_server_ipc_overrides_copy() -> None:
    from c_two._native import RuntimeSession

    overrides = {'pool_segment_size': 2 * 1024 * 1024}
    session = RuntimeSession(
        server_id='unit-server',
        server_ipc_overrides=overrides,
    )

    projected = session.server_ipc_overrides
    assert projected == overrides

    overrides['pool_segment_size'] = 4 * 1024 * 1024
    assert session.server_ipc_overrides == {'pool_segment_size': 2 * 1024 * 1024}


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


def test_runtime_session_passes_server_instance_identity_to_bridge(monkeypatch) -> None:
    captured: dict[str, object] = {}

    class FakeBridge:
        def __init__(self, **kwargs: object) -> None:
            captured.update(kwargs)

    monkeypatch.setattr(
        'c_two.transport.server.native.NativeServerBridge',
        FakeBridge,
    )

    from c_two._native import RuntimeSession

    session = RuntimeSession(shm_threshold=4096)
    session.ensure_server_bridge()

    assert isinstance(captured['server_id'], str)
    assert isinstance(captured['server_instance_id'], str)
    assert captured['server_id']
    assert captured['server_instance_id']
    assert captured['server_id'] != captured['server_instance_id']


def test_failed_server_construction_does_not_publish_identity() -> None:
    import c_two as cc

    @cc.crm(namespace='cc.test.runtime_session', version='0.1.0')
    class BrokenConfigCRM:
        def ping(self) -> str:
            ...

    class BrokenConfigImpl:
        def ping(self) -> str:
            return 'pong'

    cc.shutdown()
    cc.set_server(ipc_overrides={'pool_segment_size': 0})
    try:
        with pytest.raises(ValueError):
            cc.register(BrokenConfigCRM, BrokenConfigImpl(), name='broken-config')
        assert cc.server_id() is None
        assert cc.server_address() is None
    finally:
        cc.shutdown()


def test_explicit_ipc_connect_ignores_bad_relay_after_native_identity() -> None:
    import c_two as cc
    from c_two.config import settings

    @cc.crm(namespace='cc.test.runtime_session_bad_relay', version='0.1.0')
    class BadRelayCRM:
        def ping(self) -> str:
            ...

    class BadRelayImpl:
        def ping(self) -> str:
            return 'pong'

    cc.shutdown()
    previous = settings.relay_anchor_address
    try:
        settings.relay_anchor_address = None
        cc.register(BadRelayCRM, BadRelayImpl(), name='bad-relay-route')
        address = cc.server_address()
        assert address is not None

        settings.relay_anchor_address = 'http://127.0.0.1:9'
        crm = cc.connect(BadRelayCRM, name='bad-relay-route', address=address)
        try:
            assert crm.ping() == 'pong'
        finally:
            cc.close(crm)
    finally:
        settings.relay_anchor_address = previous
        cc.shutdown()


def test_name_only_connect_without_relay_ignores_bad_relay_proxy_env(monkeypatch) -> None:
    import c_two as cc
    from c_two.config import settings

    @cc.crm(namespace='cc.test.runtime_session_no_relay_bad_proxy', version='0.1.0')
    class NoRelayBadProxyCRM:
        def ping(self) -> str:
            ...

    cc.shutdown()
    previous = settings.relay_anchor_address
    monkeypatch.setenv('C2_ENV_FILE', '')
    monkeypatch.setenv('C2_RELAY_USE_PROXY', 'not-a-bool')
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    try:
        settings.relay_anchor_address = None
        with pytest.raises(LookupError, match='not registered locally'):
            cc.connect(NoRelayBadProxyCRM, name='missing-no-relay')
    finally:
        settings.relay_anchor_address = previous
        cc.shutdown()


def test_no_relay_register_unregister_ignores_bad_relay_proxy_env(monkeypatch) -> None:
    import c_two as cc
    from c_two.config import settings

    @cc.crm(namespace='cc.test.runtime_session_bad_proxy_env', version='0.1.0')
    class BadProxyEnvCRM:
        def ping(self) -> str:
            ...

    class BadProxyEnvImpl:
        def ping(self) -> str:
            return 'pong'

    cc.shutdown()
    previous = settings.relay_anchor_address
    monkeypatch.setenv('C2_ENV_FILE', '')
    monkeypatch.setenv('C2_RELAY_USE_PROXY', 'not-a-bool')
    try:
        settings.relay_anchor_address = None
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
        settings.relay_anchor_address = previous
        cc.shutdown()


def test_shutdown_records_bad_relay_env_file_and_closes_server(
    monkeypatch,
    tmp_path,
    caplog,
) -> None:
    import c_two as cc
    from c_two.config import settings

    @cc.crm(namespace='cc.test.runtime_session_bad_shutdown_env', version='0.1.0')
    class BadShutdownEnvCRM:
        def ping(self) -> str:
            ...

    class BadShutdownEnvImpl:
        def ping(self) -> str:
            return 'pong'

    cc.shutdown()
    previous = settings.relay_anchor_address
    bad_env_dir = tmp_path / 'env-dir'
    bad_env_dir.mkdir()
    try:
        settings.relay_anchor_address = None
        cc.register(BadShutdownEnvCRM, BadShutdownEnvImpl(), name='bad-shutdown-env')
        assert cc.server_address() is not None

        monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
        monkeypatch.setenv('C2_ENV_FILE', str(bad_env_dir))
        with caplog.at_level(logging.INFO):
            cc.shutdown()

        assert cc.server_address() is None
        assert any(
            'Relay unreachable during shutdown unregister of bad-shutdown-env' in record.message
            and 'env file' in record.message
            for record in caplog.records
        )
    finally:
        settings.relay_anchor_address = previous
        cc.shutdown()


def test_shutdown_records_bad_relay_proxy_env_and_closes_server(
    monkeypatch,
    caplog,
) -> None:
    import c_two as cc
    from c_two.config import settings

    @cc.crm(namespace='cc.test.runtime_session_bad_shutdown_proxy', version='0.1.0')
    class BadShutdownProxyCRM:
        def ping(self) -> str:
            ...

    class BadShutdownProxyImpl:
        def ping(self) -> str:
            return 'pong'

    cc.shutdown()
    previous = settings.relay_anchor_address
    try:
        monkeypatch.setenv('C2_ENV_FILE', '')
        monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
        settings.relay_anchor_address = None
        cc.register(BadShutdownProxyCRM, BadShutdownProxyImpl(), name='bad-shutdown-proxy')
        assert cc.server_address() is not None

        settings.relay_anchor_address = 'http://127.0.0.1:9'
        monkeypatch.setenv('C2_RELAY_USE_PROXY', 'not-a-bool')
        with caplog.at_level(logging.INFO):
            cc.shutdown()

        assert cc.server_address() is None
        assert any(
            'Relay unreachable during shutdown unregister of bad-shutdown-proxy' in record.message
            and 'C2_RELAY_USE_PROXY' in record.message
            for record in caplog.records
        )
    finally:
        settings.relay_anchor_address = previous
        cc.shutdown()


def test_failed_first_route_registration_clears_native_identity(monkeypatch) -> None:
    import c_two as cc
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
        assert cc.server_id() is None
        assert cc.server_address() is None
    finally:
        cc.shutdown()


def test_failed_server_bridge_construction_clears_native_identity(monkeypatch) -> None:
    import c_two as cc
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
        assert cc.server_id() is None
        assert cc.server_address() is None
    finally:
        cc.shutdown()


def test_registry_does_not_construct_server_directly() -> None:
    import inspect
    from c_two.transport import registry

    source = inspect.getsource(registry)
    assert 'self._server = Server(' not in source
    assert 'server = Server(' not in source


def test_native_runtime_session_register_errors_expose_structured_failure_attrs() -> None:
    repo_root = Path(__file__).resolve().parents[4]
    source = (
        repo_root / 'sdk/python/native/src/runtime_session_ffi.rs'
    ).read_text(encoding='utf-8')

    assert 'registration_failure' in source
    assert 'setattr("rollback"' in source
    assert 'setattr("relay_cleanup_error"' in source
    assert 'relay_cleanup_error_to_dict' in source
    assert 'route_close_outcome_to_dict(py, rollback)' in source
    assert 'registration_rollback_to_dict' not in source
    assert 'RegisterFailure(' in source


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
        from c_two.transport.registry import _ProcessRegistry
        registry = _ProcessRegistry.get()
        assert registry.names == ['good']
        crm = registry.connect(Phase3CRM, name='good')
        try:
            assert crm.ping() == 'good'
        finally:
            registry.close(crm)
    finally:
        cc.shutdown()


def test_failed_native_registration_leaves_no_route_or_identity(monkeypatch) -> None:
    import c_two as cc
    from c_two.transport.server.native import NativeServerBridge
    from c_two.transport.registry import _ProcessRegistry

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

        registry = _ProcessRegistry.get()
        assert registry.names == []
        assert registry.get_server_address() is None
        assert registry.get_server_id() is None
        with pytest.raises(LookupError, match='not registered locally'):
            registry.connect(Phase3FirstCRM, name='first')
    finally:
        cc.shutdown()


def test_native_runtime_session_projects_client_ipc_overrides_copy() -> None:
    from c_two._native import RuntimeSession

    overrides = {'reassembly_segment_size': 16 * 1024 * 1024}
    session = RuntimeSession(client_ipc_overrides=overrides)

    assert session.client_ipc_overrides == overrides
    overrides['reassembly_segment_size'] = 32 * 1024 * 1024
    assert session.client_ipc_overrides == {
        'reassembly_segment_size': 16 * 1024 * 1024,
    }


def test_native_runtime_session_resolves_client_ipc_config(monkeypatch) -> None:
    from c_two._native import RuntimeSession

    monkeypatch.setenv('C2_ENV_FILE', '')
    monkeypatch.setenv('C2_IPC_REASSEMBLY_SEGMENT_SIZE', str(32 * 1024 * 1024))
    session = RuntimeSession(
        client_ipc_overrides={'reassembly_segment_size': 16 * 1024 * 1024},
    )

    snapshot = session.client_ipc_config
    assert snapshot['reassembly_segment_size'] == 16 * 1024 * 1024


def test_set_client_preserves_pending_server_id_override() -> None:
    import c_two as cc

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
        cc.set_client(ipc_overrides={'reassembly_segment_size': 16 * 1024 * 1024})
        cc.register(PreserveCRM, PreserveImpl(), name='preserve-route')
        assert cc.server_id() == 'unit-preserve-server'
        assert cc.server_address() == 'ipc://unit-preserve-server'
    finally:
        cc.shutdown()


def test_set_server_preserves_pending_client_overrides() -> None:
    import c_two as cc
    from c_two.transport.registry import _ProcessRegistry

    cc.shutdown()
    try:
        cc.set_client(ipc_overrides={'reassembly_segment_size': 16 * 1024 * 1024})
        cc.set_server(server_id='unit-preserve-client')
        registry = _ProcessRegistry.get()
        assert registry._runtime_session.client_ipc_config['reassembly_segment_size'] == 16 * 1024 * 1024  # noqa: SLF001
    finally:
        cc.shutdown()


def test_set_client_after_direct_ipc_connect_warns() -> None:
    import pytest
    import c_two as cc

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
            with pytest.warns(UserWarning, match='Client connections already exist'):
                cc.set_client(ipc_overrides={'reassembly_segment_size': 16 * 1024 * 1024})
        finally:
            cc.close(crm)
    finally:
        cc.shutdown()


def test_set_client_after_register_preserves_server_identity() -> None:
    import c_two as cc

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
        cc.set_client(ipc_overrides={'reassembly_segment_size': 16 * 1024 * 1024})
        assert cc.server_id() == 'unit-after-register'
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


def test_runtime_session_client_config_respects_transport_policy() -> None:
    import c_two as cc
    from c_two.transport.registry import _ProcessRegistry

    cc.shutdown()
    try:
        cc.set_transport_policy(shm_threshold=16384)
        registry = _ProcessRegistry.get()
        assert registry._runtime_session.client_ipc_config['shm_threshold'] == 16384  # noqa: SLF001
    finally:
        cc.shutdown()


def test_set_transport_policy_after_direct_ipc_connect_warns() -> None:
    import pytest
    import c_two as cc

    @cc.crm(namespace='cc.test.runtime_session_policy_freeze', version='0.1.0')
    class PolicyCRM:
        def ping(self) -> str:
            ...

    class PolicyImpl:
        def ping(self) -> str:
            return 'pong'

    cc.shutdown()
    try:
        cc.register(PolicyCRM, PolicyImpl(), name='policy-route')
        address = cc.server_address()
        crm = cc.connect(PolicyCRM, name='policy-route', address=address)
        try:
            with pytest.warns(UserWarning, match='Active connections exist'):
                cc.set_transport_policy(shm_threshold=16384)
        finally:
            cc.close(crm)
    finally:
        cc.shutdown()


def test_set_transport_policy_after_client_only_ipc_connect_warns() -> None:
    import pytest
    import c_two as cc
    from c_two.transport import Server

    @cc.crm(namespace='cc.test.runtime_session_policy_client_only', version='0.1.0')
    class ClientOnlyCRM:
        def ping(self) -> str:
            ...

    class ClientOnlyImpl:
        def ping(self) -> str:
            return 'pong'

    address = 'ipc://unit_policy_client_only'
    server = Server(bind_address=address)
    server.register_crm(ClientOnlyCRM, ClientOnlyImpl(), name='policy-client-only')
    server.start()
    cc.shutdown()
    try:
        crm = cc.connect(ClientOnlyCRM, name='policy-client-only', address=address)
        try:
            with pytest.warns(UserWarning, match='Active connections exist'):
                cc.set_transport_policy(shm_threshold=16384)
        finally:
            cc.close(crm)
    finally:
        server.shutdown()
        cc.shutdown()


def test_set_server_after_client_only_connect_preserves_client_freeze() -> None:
    import pytest
    import c_two as cc
    from c_two.transport import Server

    @cc.crm(namespace='cc.test.runtime_session_server_after_client', version='0.1.0')
    class ServerAfterClientCRM:
        def ping(self) -> str:
            ...

    class ServerAfterClientImpl:
        def ping(self) -> str:
            return 'pong'

    address = 'ipc://unit_server_after_client'
    server = Server(bind_address=address)
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
            with pytest.warns(UserWarning, match='Client connections already exist'):
                cc.set_client(ipc_overrides={'reassembly_segment_size': 16 * 1024 * 1024})
        finally:
            cc.close(crm)
    finally:
        server.shutdown()
        cc.shutdown()


def test_runtime_session_shutdown_drains_explicit_http_pool(monkeypatch) -> None:
    from c_two._native import RuntimeSession

    monkeypatch.setenv('C2_ENV_FILE', '')
    monkeypatch.setenv('C2_RELAY_USE_PROXY', 'false')
    session = RuntimeSession()
    relay_url = 'http://127.0.0.1:9'

    session.acquire_http_client(relay_url)
    assert session.http_client_refcount(relay_url) == 1

    outcome = dict(session.shutdown(None, route_names=[]))

    assert outcome['http_clients_drained'] is True
    assert outcome['route_outcomes'] == []
    assert outcome['route_close_error'] is None
    assert outcome['runtime_barrier_error'] is None
    assert session.http_client_refcount(relay_url) == 0


def test_set_relay_anchor_blank_clears_native_override() -> None:
    from c_two.config.settings import settings
    from c_two.transport.registry import _ProcessRegistry

    registry = _ProcessRegistry.get()
    previous = settings._relay_anchor_address  # noqa: SLF001
    try:
        registry.set_relay_anchor('http://relay.test/')
        assert registry._runtime_session.relay_anchor_address_override == 'http://relay.test'  # noqa: SLF001

        registry.set_relay_anchor('   ')

        assert settings._relay_anchor_address is None  # noqa: SLF001
        assert registry._runtime_session.relay_anchor_address_override is None  # noqa: SLF001
    finally:
        settings.relay_anchor_address = previous
        registry.shutdown()


def test_relay_ipc_acceptance_does_not_trust_route_name_only() -> None:
    repo_root = Path(__file__).resolve().parents[4]
    source = (
        repo_root / 'sdk/python/native/src/runtime_session_ffi.rs'
    ).read_text(encoding='utf-8')
    acquire_body = source.split('fn acquire_relay_ipc_client(', 1)[1].split(
        'fn acquire_relay_http_client(',
        1,
    )[0]

    assert 'expected_server_id' in acquire_body
    assert 'expected_server_instance_id' in acquire_body
    identity_checks = [
        pos for needle in ('server_identity()', 'server_instance_id()')
        if (pos := acquire_body.find(needle)) >= 0
    ]
    assert identity_checks
    assert min(identity_checks) < acquire_body.find('route_names()')


def test_relay_ipc_identity_mismatch_falls_back_to_http_not_hard_error() -> None:
    repo_root = Path(__file__).resolve().parents[4]
    source = (
        repo_root / 'sdk/python/native/src/runtime_session_ffi.rs'
    ).read_text(encoding='utf-8')
    connect_body = source.split('fn connect_via_relay(', 1)[1].split(
        'fn shutdown_ipc_clients(',
        1,
    )[0]

    assert 'RelayIpcConnectError::Unavailable' in connect_body
    assert 'acquire_relay_http_client' in connect_body
    assert (
        connect_body.find('RelayIpcConnectError::Unavailable')
        < connect_body.find('acquire_relay_http_client')
    )


def test_relay_ipc_unavailable_reason_is_logged_before_http_fallback() -> None:
    repo_root = Path(__file__).resolve().parents[4]
    source = (
        repo_root / 'sdk/python/native/src/runtime_session_ffi.rs'
    ).read_text(encoding='utf-8')
    connect_body = source.split('fn connect_via_relay(', 1)[1].split(
        'fn shutdown_ipc_clients(',
        1,
    )[0]
    acquire_body = source.split('fn acquire_relay_ipc_client(', 1)[1].split(
        'fn acquire_relay_http_client(',
        1,
    )[0]

    assert 'RelayIpcUnavailableReason::PoolAcquire' in source
    assert 'RelayIpcUnavailableReason::IdentityMismatch' in source
    assert 'RelayIpcUnavailableReason::RouteMissing' in source
    assert 'Err(RelayIpcConnectError::Unavailable(reason))' in connect_body
    assert 'eprintln!' in connect_body
    assert (
        connect_body.find('eprintln!')
        < connect_body.find('acquire_relay_http_client')
    )
    assert 'RelayIpcUnavailable::pool_acquire' in acquire_body
    assert 'RelayIpcUnavailable::identity_mismatch' in acquire_body
    assert 'RelayIpcUnavailable::route_missing' in acquire_body


def test_relay_ipc_identity_boundary_is_native_owned() -> None:
    repo_root = Path(__file__).resolve().parents[4]
    registry_source = (
        repo_root / 'sdk/python/src/c_two/transport/registry.py'
    ).read_text(encoding='utf-8')
    native_source = (
        repo_root / 'sdk/python/native/src/runtime_session_ffi.rs'
    ).read_text(encoding='utf-8')

    assert 'server_instance_id' not in registry_source
    assert 'expected_server_instance_id' in native_source
    assert 'route_names()' in native_source
