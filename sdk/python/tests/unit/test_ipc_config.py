"""Tests for Host identity and relay configuration projections."""
from __future__ import annotations

import inspect
from pathlib import Path

import pytest

import c_two as cc
from c_two import _native

import c_two.endpoint
from c_two.config.settings import settings
from c_two.error import ResourceNotFound
from c_two.transport import Server
from c_two.transport.registry import _get_default_runtime, _reset_default_runtime


@cc.crm(namespace='unit.config', version='0.1.0')
class IUnitConfigCRM:
    def ping(self) -> str:
        ...


class UnitConfigCRM:
    def ping(self) -> str:
        return 'pong'


@pytest.fixture(autouse=True)
def _reset_registry_and_settings(monkeypatch):
    _reset_default_runtime()
    monkeypatch.setenv('C2_ENV_FILE', '')
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', '')
    monkeypatch.setattr(settings, '_relay_anchor_address', None)
    yield
    _reset_default_runtime()


def test_server_id_override_drives_auto_address():
    cc.set_server(server_id='unit-server')
    cc.register(IUnitConfigCRM, UnitConfigCRM(), name='unit-route')
    try:
        assert cc.endpoint.server_id() == 'unit-server'
        assert cc.server_address() == 'ipc://unit-server'
    finally:
        cc.unregister('unit-route')


def test_server_id_rejects_empty_or_path_like_values():
    with pytest.raises(ValueError, match='server_id cannot be empty'):
        cc.set_server(server_id='')

    with pytest.raises(ValueError, match='leading or trailing whitespace'):
        cc.set_server(server_id=' ')

    with pytest.raises(ValueError, match='leading or trailing whitespace'):
        cc.set_server(server_id=' unit ')

    with pytest.raises(ValueError, match='path separators'):
        cc.set_server(server_id='bad/name')

    with pytest.raises(ValueError, match='path separators'):
        cc.set_server(server_id='.')

    with pytest.raises(ValueError, match='path separators'):
        cc.set_server(server_id='..')

    with pytest.raises(ValueError, match='control characters'):
        cc.set_server(server_id='bad\nid')


class _FakeRetiredObservation:
    """Retirement-bundle double for registry test doubles.

    The real observation is an opaque handoff token: its lifetime is decided
    by real owners inside Rust, so the session interface has no fence to
    model.
    """


class _FakeRuntimeSession:
    """Only the native projections used by these route/teardown spies.

    Contexts are real opaque native values; the double derives no endpoint
    paths and invents no default execution policy.
    """

    client_config_frozen = False
    server_id = None
    server_id_override = None

    def __init__(
        self,
        *,
        max_outstanding_calls: int | None = None,
        memory_budget_bytes: int | None = None,
        **_kwargs,
    ) -> None:
        from c_two._native import resolve_local_endpoint_context

        self._context = resolve_local_endpoint_context()
        self._limits = {
            key: value for key, value in (
                ('max_outstanding_calls', max_outstanding_calls),
                ('memory_budget_bytes', memory_budget_bytes),
            ) if value is not None
        }
        self.adopted_observations: list[_FakeRetiredObservation] = []

    @property
    def call_execution_limits_overrides(self) -> dict[str, int]:
        return dict(self._limits)

    def local_endpoint_context(self) -> cc.endpoint.LocalEndpointContext:
        return self._context

    def inherit_local_endpoint_selection(self, previous: _FakeRuntimeSession) -> None:
        self._context = previous._context

    retired = False

    def check_active(self, *, connect_attempt=None):
        pass  # These are routing/configuration spies, not lifecycle doubles.

    def begin_shutdown(self, *, timeout_seconds=5.0):
        return self

    def observe(self):
        return {'completed': True, 'relay_errors': [], 'route_outcomes': []}

    def inherit_retirement(self, previous: _FakeRuntimeSession) -> None:
        self._limits = previous._limits
        self.adopted_observations.append(_FakeRetiredObservation())

    def set_relay_anchor_address(self, relay_address: str | None, *, connect_attempt=None) -> None:
        self.relay_anchor_address_override = relay_address
        self.connect_attempt = connect_attempt

    def local_copy_tracker(self):
        return None


def test_relay_resolved_connect_delegates_route_validation_to_runtime_session(monkeypatch):
    registry = _get_default_runtime()
    calls = []

    class FakeRuntimeSession(_FakeRuntimeSession):
        def connect_via_relay(
            self,
            route_name: str,
            expected_crm_ns: str,
            expected_crm_name: str,
            expected_crm_ver: str,
            expected_abi_hash: str,
            expected_signature_hash: str,
            *,
            connect_attempt,
        ):
            assert connect_attempt is self.connect_attempt
            calls.append((
                self.relay_anchor_address_override,
                route_name,
                expected_crm_ns,
                expected_crm_name,
                expected_crm_ver,
                expected_abi_hash,
                expected_signature_hash,
            ))
            return FakeRelayAwareClient()

    class FakeRelayAwareClient:
        def close(self):
            pass

    with monkeypatch.context() as patch:
        patch.setattr(settings, 'relay_anchor_address', 'http://registry-relay.test')
        patch.setattr(registry, '_runtime_session', FakeRuntimeSession())

        crm = registry.connect(IUnitConfigCRM, name='unit-route')

        assert 'RustClientPool.instance()' not in inspect.getsource(type(registry))
        assert len(calls) == 1
        relay_address, route_name, crm_ns, crm_name, crm_ver, abi_hash, signature_hash = calls[0]
        assert (relay_address, route_name, crm_ns, crm_name, crm_ver) == (
            'http://registry-relay.test',
            'unit-route',
            'unit.config',
            'IUnitConfigCRM',
            '0.1.0',
        )
        assert len(abi_hash) == 64
        assert len(signature_hash) == 64
        assert crm.client._client.__class__ is FakeRelayAwareClient  # noqa: SLF001


def test_route_spy_shutdown_preserves_native_context_and_accepted_limits(monkeypatch):
    registry = _get_default_runtime()
    installed = _FakeRuntimeSession(
        max_outstanding_calls=7, memory_budget_bytes=8192,
    )
    captured = installed.local_endpoint_context()

    with monkeypatch.context() as patch:
        patch.setattr(registry, '_runtime_session', installed)
        assert registry.shutdown()['completed'] is True
        replacement = registry._runtime_session  # noqa: SLF001
        assert replacement is not installed
        assert replacement.local_endpoint_context() is captured
        assert replacement.call_execution_limits_overrides == {
            'max_outstanding_calls': 7, 'memory_budget_bytes': 8192,
        }
        assert len(replacement.adopted_observations) == 1
        assert isinstance(replacement.adopted_observations[0], _FakeRetiredObservation)


def test_relay_resolved_connect_uses_native_session_not_python_relay_client():
    registry = _get_default_runtime()
    settings.relay_anchor_address = 'http://registry-relay.test'

    with pytest.raises(Exception):
        registry.connect(IUnitConfigCRM, name='unit-route')

    source = inspect.getsource(type(registry))
    assert 'RustRelayAwareHttpClient(' not in source
    assert '_relay_control_client_for' not in source


def test_relay_connected_http_mode_uses_single_core_client_call_path():
    import re

    repo_root = Path(__file__).resolve().parents[4]
    runtime_source = (
        repo_root / 'sdk/python/native/src/runtime_session_ffi.rs'
    ).read_text(encoding='utf-8')
    client_source = (
        repo_root / 'sdk/python/native/src/core_ffi.rs'
    ).read_text(encoding='utf-8')

    assert 'Connect::RelayAware' in runtime_source
    assert re.search(
        r'self\s*\.\s*connect_core\s*\(\s*py\s*,\s*expected\s*,\s*Connect::RelayAware\s*,'
        r'\s*timeout_seconds\s*,\s*connect_attempt\s*,?\s*\)',
        runtime_source,
    )
    assert re.search(r'inner:\s*Mutex\s*<\s*Option\s*<\s*Client\s*>\s*>', client_source)
    assert 'RelayAwareHttpClient' not in runtime_source
    assert 'release_http_client_from_global_pool' not in client_source


def test_relay_resolved_connect_maps_native_404_to_resource_not_found(monkeypatch):
    registry = _get_default_runtime()

    class FakeRuntimeSession(_FakeRuntimeSession):
        def connect_via_relay(
            self,
            route_name: str,
            expected_crm_ns: str,
            expected_crm_name: str,
            expected_crm_ver: str,
            expected_abi_hash: str,
            expected_signature_hash: str,
            *,
            connect_attempt,
        ):  # noqa: ARG002
            assert connect_attempt is self.connect_attempt
            err = RuntimeError('HTTP 404')
            err.status_code = 404
            raise err

    with monkeypatch.context() as patch:
        patch.setattr(settings, 'relay_anchor_address', 'http://registry-relay.test')
        patch.setattr(registry, '_runtime_session', FakeRuntimeSession())

        with pytest.raises(ResourceNotFound, match="Resource 'unit-route' not found"):
            registry.connect(IUnitConfigCRM, name='unit-route')


@pytest.mark.parametrize('key', [
    'bind_address', 'ipc_overrides', 'server_id', 'server_instance_id',
    'ipc_config', 'shm_threshold',
])
def test_removed_low_level_server_parameters_are_rejected(key):
    with pytest.raises(TypeError, match=key):
        Server(object(), **{key: object()})


def test_removed_high_level_flat_config_api_is_rejected():
    assert not hasattr(cc, 'set_config')

    with pytest.raises(TypeError, match='pool_segment_size'):
        cc.set_server(pool_segment_size=1024)

    with pytest.raises(TypeError, match='pool_segment_size'):
        cc.set_client(pool_segment_size=1024)


def test_proxy_policy_reads_env_file(tmp_path, monkeypatch):
    env_file = tmp_path / '.env'
    env_file.write_text('C2_RELAY_USE_PROXY=1\n', encoding='utf-8')
    monkeypatch.setenv('C2_ENV_FILE', str(env_file))
    monkeypatch.delenv('C2_RELAY_USE_PROXY', raising=False)

    from c_two._native import resolve_relay_use_proxy

    assert resolve_relay_use_proxy() is True


def test_proxy_policy_does_not_resolve_ipc_env(monkeypatch):
    monkeypatch.setenv('C2_RELAY_USE_PROXY', '1')
    monkeypatch.setenv('C2_MEMORY_BUDGET_BYTES', 'not-a-number')

    from c_two._native import resolve_relay_use_proxy

    assert resolve_relay_use_proxy() is True


def test_relay_settings_do_not_resolve_ipc_env(monkeypatch):
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', 'http://127.0.0.1:8080')
    monkeypatch.setenv('C2_MEMORY_BUDGET_BYTES', 'not-a-number')

    assert settings.relay_anchor_address == 'http://127.0.0.1:8080'


def test_relay_anchor_address_resolution_ignores_relay_proxy_policy(monkeypatch):
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', 'http://127.0.0.1:8080')
    monkeypatch.setenv('C2_RELAY_USE_PROXY', 'not-a-bool')

    assert settings.relay_anchor_address == 'http://127.0.0.1:8080'


def test_public_relay_anchor_api_replaces_set_relay():
    assert hasattr(cc, 'set_relay_anchor')
    assert not hasattr(cc, 'set_relay')
