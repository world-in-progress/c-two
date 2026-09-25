"""Unit tests for the Python IPC override facade."""
from __future__ import annotations

import inspect
from collections import UserDict
from pathlib import Path
from typing import get_type_hints

import pytest

import c_two as cc
import c_two.config as config
import c_two.config.ipc as ipc_config
from c_two.config.settings import settings
from c_two.error import ResourceNotFound
from c_two.transport import Server
from c_two.transport.registry import _ProcessRegistry


@cc.crm(namespace='unit.config', version='0.1.0')
class IUnitConfigCRM:
    def ping(self) -> str:
        ...


class UnitConfigCRM:
    def ping(self) -> str:
        return 'pong'


@pytest.fixture(autouse=True)
def _reset_registry_and_settings(monkeypatch):
    cc.shutdown()
    _ProcessRegistry.reset()
    settings.relay_anchor_address = None
    settings.shm_threshold = None
    settings.remote_payload_chunk_size = None
    monkeypatch.setenv('C2_ENV_FILE', '')
    for key in (
        'C2_SHM_THRESHOLD',
        'C2_REMOTE_PAYLOAD_CHUNK_SIZE',
        'C2_IPC_POOL_SEGMENT_SIZE',
        'C2_IPC_REASSEMBLY_SEGMENT_SIZE',
        'C2_RELAY_ANCHOR_ADDRESS',
        'C2_RELAY_USE_PROXY',
        'C2_RELAY_ROUTE_MAX_ATTEMPTS',
    ):
        monkeypatch.delenv(key, raising=False)
    yield
    cc.shutdown()
    _ProcessRegistry.reset()
    settings.relay_anchor_address = None
    settings.shm_threshold = None
    settings.remote_payload_chunk_size = None


def test_public_config_exports_only_override_schemas():
    removed = {
        'BaseIPCConfig',
        'ServerIPCConfig',
        'ClientIPCConfig',
        'build_server_config',
        'build_client_config',
    }
    for name in removed:
        assert not hasattr(config, name)

    assert hasattr(config, 'BaseIPCOverrides')
    assert hasattr(config, 'ServerIPCOverrides')
    assert hasattr(config, 'ClientIPCOverrides')


def test_ipc_module_exports_only_override_schemas():
    assert set(ipc_config.__all__) == {
        'BaseIPCOverrides',
        'ServerIPCOverrides',
        'ClientIPCOverrides',
    }


def test_override_schemas_are_typed_and_do_not_include_derived_or_global_fields():
    base = get_type_hints(config.BaseIPCOverrides)
    server = get_type_hints(config.ServerIPCOverrides)
    client = get_type_hints(config.ClientIPCOverrides)

    assert base['pool_enabled'] is bool
    assert base['pool_segment_size'] is int
    assert base['chunk_gc_interval'] is float
    assert server['heartbeat_interval'] is float
    assert server['max_pending_requests'] is int
    assert server['max_execution_workers'] is int
    assert client['reassembly_segment_size'] is int

    for hints in (base, server, client):
        assert 'shm_threshold' not in hints
        assert 'max_pool_memory' not in hints


def test_low_level_server_uses_env_for_ipc_defaults(monkeypatch):
    monkeypatch.setenv('C2_IPC_POOL_SEGMENT_SIZE', str(2 * 1024 * 1024))

    server = Server(bind_address='ipc://unit_env_server')
    try:
        assert server._config['pool_segment_size'] == 2 * 1024 * 1024  # noqa: SLF001
    finally:
        server.shutdown()


def test_low_level_server_uses_env_for_global_transport_policy(monkeypatch):
    monkeypatch.setenv('C2_SHM_THRESHOLD', '8192')

    server = Server(bind_address='ipc://unit_env_threshold')
    try:
        assert server._config['shm_threshold'] == 8192  # noqa: SLF001
    finally:
        server.shutdown()


def test_transport_policy_override_beats_env(monkeypatch):
    monkeypatch.setenv('C2_SHM_THRESHOLD', '8192')

    cc.set_transport_policy(shm_threshold=16384)

    server = Server(bind_address='ipc://unit_policy_override')
    try:
        assert server._config['shm_threshold'] == 16384  # noqa: SLF001
    finally:
        server.shutdown()


def test_remote_payload_chunk_size_resolves_from_env_and_policy(monkeypatch):
    monkeypatch.setenv('C2_REMOTE_PAYLOAD_CHUNK_SIZE', '2097152')

    from c_two._native import resolve_remote_payload_chunk_size

    assert resolve_remote_payload_chunk_size() == 2_097_152
    assert settings.remote_payload_chunk_size == 2_097_152

    cc.set_transport_policy(remote_payload_chunk_size=1_048_576)
    registry = _ProcessRegistry.get()

    assert settings.remote_payload_chunk_size == 1_048_576
    assert registry._runtime_session.remote_payload_chunk_size_override == 1_048_576  # noqa: SLF001


def test_transport_policy_updates_preserve_unspecified_overrides():
    cc.set_transport_policy(shm_threshold=8192)
    cc.set_transport_policy(remote_payload_chunk_size=1_048_576)
    registry = _ProcessRegistry.get()

    assert settings.shm_threshold == 8192
    assert settings.remote_payload_chunk_size == 1_048_576
    assert registry._runtime_session.client_ipc_config['shm_threshold'] == 8192  # noqa: SLF001
    assert registry._runtime_session.remote_payload_chunk_size_override == 1_048_576  # noqa: SLF001

    cc.set_transport_policy(shm_threshold=None)
    registry = _ProcessRegistry.get()

    assert registry._runtime_session.client_ipc_config['shm_threshold'] == 4096  # noqa: SLF001
    assert registry._runtime_session.remote_payload_chunk_size_override == 1_048_576  # noqa: SLF001


def test_remote_payload_chunk_size_zero_rejected(monkeypatch):
    monkeypatch.setenv('C2_REMOTE_PAYLOAD_CHUNK_SIZE', '0')

    from c_two._native import resolve_remote_payload_chunk_size

    with pytest.raises(ValueError, match='C2_REMOTE_PAYLOAD_CHUNK_SIZE'):
        resolve_remote_payload_chunk_size()

    monkeypatch.delenv('C2_REMOTE_PAYLOAD_CHUNK_SIZE')
    with pytest.raises(ValueError, match='C2_REMOTE_PAYLOAD_CHUNK_SIZE'):
        cc.set_transport_policy(remote_payload_chunk_size=0)


def test_shm_override_helper_only_contains_shm_threshold():
    settings.shm_threshold = 4096
    assert settings._shm_overrides() == {'shm_threshold': 4096}  # noqa: SLF001


def test_server_ipc_overrides_beat_env(monkeypatch):
    monkeypatch.setenv('C2_IPC_POOL_SEGMENT_SIZE', str(2 * 1024 * 1024))

    server = Server(
        bind_address='ipc://unit_server_override',
        ipc_overrides={'pool_segment_size': 4 * 1024 * 1024},
    )
    try:
        assert server._config['pool_segment_size'] == 4 * 1024 * 1024  # noqa: SLF001
    finally:
        server.shutdown()


def test_server_execution_workers_env_and_override(monkeypatch):
    monkeypatch.setenv('C2_IPC_MAX_EXECUTION_WORKERS', '9')

    server = Server(bind_address='ipc://unit_execution_workers_env')
    try:
        assert server._config['max_execution_workers'] == 9  # noqa: SLF001
    finally:
        server.shutdown()

    override_server = Server(
        bind_address='ipc://unit_execution_workers_override',
        ipc_overrides={'max_execution_workers': 7},
    )
    try:
        assert override_server._config['max_execution_workers'] == 7  # noqa: SLF001
    finally:
        override_server.shutdown()


def test_server_execution_workers_zero_rejected(monkeypatch):
    monkeypatch.setenv('C2_IPC_MAX_EXECUTION_WORKERS', '0')

    with pytest.raises(ValueError, match='max_execution_workers'):
        Server(bind_address='ipc://unit_execution_workers_zero_env')

    monkeypatch.delenv('C2_IPC_MAX_EXECUTION_WORKERS')
    with pytest.raises(ValueError, match='max_execution_workers'):
        Server(
            bind_address='ipc://unit_execution_workers_zero_override',
            ipc_overrides={'max_execution_workers': 0},
        )


def test_server_execution_workers_above_hard_limit_rejected(monkeypatch):
    monkeypatch.setenv('C2_IPC_MAX_EXECUTION_WORKERS', '65')

    with pytest.raises(ValueError, match='max_execution_workers'):
        Server(bind_address='ipc://unit_execution_workers_oversized_env')

    monkeypatch.delenv('C2_IPC_MAX_EXECUTION_WORKERS')
    with pytest.raises(ValueError, match='max_execution_workers'):
        Server(
            bind_address='ipc://unit_execution_workers_oversized_override',
            ipc_overrides={'max_execution_workers': 65},
        )


def test_client_ipc_overrides_beat_env(monkeypatch):
    monkeypatch.setenv('C2_IPC_REASSEMBLY_SEGMENT_SIZE', str(32 * 1024 * 1024))

    cc.set_client(ipc_overrides={'reassembly_segment_size': 16 * 1024 * 1024})
    registry = _ProcessRegistry.get()

    assert registry._runtime_session.client_ipc_config['reassembly_segment_size'] == 16 * 1024 * 1024  # noqa: SLF001


@pytest.mark.parametrize('key', ['shm_threshold'])
def test_forbidden_server_ipc_override_fields_are_rejected_by_native(key):
    with pytest.raises(ValueError, match='shm_threshold is a global transport policy'):
        Server(bind_address='ipc://unit_bad_server_override', ipc_overrides={key: 1})


@pytest.mark.parametrize('key', ['shm_threshold'])
def test_forbidden_client_ipc_override_fields_are_rejected_by_native(key):
    with pytest.raises(ValueError, match='shm_threshold is a global transport policy'):
        cc.set_client(ipc_overrides={key: 1})


def test_removed_max_pool_memory_override_is_rejected_by_native():
    with pytest.raises(ValueError, match='unknown IPC override option: max_pool_memory'):
        Server(
            bind_address='ipc://unit_removed_max_pool_memory',
            ipc_overrides={'max_pool_memory': 1024},
        )

    with pytest.raises(ValueError, match='unknown IPC override option: max_pool_memory'):
        cc.set_client(ipc_overrides={'max_pool_memory': 1024})


def test_native_resolver_treats_max_pool_memory_as_unknown_override():
    native = ipc_config._native_resolver()  # noqa: SLF001

    with pytest.raises(ValueError, match='unknown IPC override option: max_pool_memory'):
        native.resolve_server_ipc_config({'max_pool_memory': 1024}, None)

    with pytest.raises(ValueError, match='unknown IPC override option: max_pool_memory'):
        native.resolve_client_ipc_config({'max_pool_memory': 1024}, None)


def test_native_resolver_accepts_mapping_subclasses():
    native = ipc_config._native_resolver()  # noqa: SLF001
    overrides = UserDict({'pool_segment_size': 2 * 1024 * 1024})

    server_cfg = native.resolve_server_ipc_config(overrides, None)
    client_cfg = native.resolve_client_ipc_config(overrides, None)

    assert server_cfg['pool_segment_size'] == 2 * 1024 * 1024
    assert client_cfg['pool_segment_size'] == 2 * 1024 * 1024


def test_unknown_ipc_override_field_is_rejected_by_native():
    with pytest.raises(ValueError, match='unknown IPC override option: not_real'):
        Server(bind_address='ipc://unit_unknown_override', ipc_overrides={'not_real': 1})

    with pytest.raises(ValueError, match='unknown IPC override option: not_real'):
        cc.set_server(ipc_overrides={'not_real': 1})

    with pytest.raises(ValueError, match='unknown IPC override option: not_real'):
        cc.set_client(ipc_overrides={'not_real': 1})


def test_ipc_overrides_must_be_mapping_native_error():
    with pytest.raises(TypeError, match='ipc_overrides must be a mapping'):
        cc.set_server(ipc_overrides=object())  # type: ignore[arg-type]

    with pytest.raises(TypeError, match='ipc_overrides must be a mapping'):
        cc.set_client(ipc_overrides=object())  # type: ignore[arg-type]


def test_ipc_override_keys_must_be_strings_native_error():
    with pytest.raises(TypeError, match='IPC override option names must be strings'):
        cc.set_client(ipc_overrides={1: 4096})  # type: ignore[dict-item]


def test_ipc_override_non_string_keys_are_shape_errors_before_semantic_errors():
    with pytest.raises(TypeError, match='IPC override option names must be strings'):
        cc.set_client(
            ipc_overrides={  # type: ignore[dict-item]
                1: 4096,
                'shm_threshold': 1,
            },
        )


def test_high_level_ipc_overrides_are_copied_when_set():
    overrides = {'pool_segment_size': 2 * 1024 * 1024}
    cc.set_server(ipc_overrides=overrides)
    overrides['pool_segment_size'] = 4 * 1024 * 1024

    registry = _ProcessRegistry.get()
    assert registry._runtime_session.server_ipc_overrides == {
        'pool_segment_size': 2 * 1024 * 1024,
    }  # noqa: SLF001


def test_ipc_override_mapping_is_snapshotted_by_native():
    overrides = UserDict({
        'pool_segment_size': 2 * 1024 * 1024,
        'chunk_size': None,
    })

    cc.set_server(ipc_overrides=overrides)  # type: ignore[arg-type]
    overrides['pool_segment_size'] = 4 * 1024 * 1024

    registry = _ProcessRegistry.get()
    assert registry._runtime_session.server_ipc_overrides == {
        'pool_segment_size': 2 * 1024 * 1024,
    }  # noqa: SLF001


def test_low_level_server_accepts_mapping_subclass_ipc_overrides():
    server = Server(
        bind_address='ipc://unit_server_mapping_subclass',
        ipc_overrides=UserDict({'pool_segment_size': 2 * 1024 * 1024}),
    )
    try:
        assert server._config['pool_segment_size'] == 2 * 1024 * 1024  # noqa: SLF001
    finally:
        server.shutdown()


def test_server_id_override_drives_auto_address():
    cc.set_server(server_id='unit-server')
    cc.register(IUnitConfigCRM, UnitConfigCRM(), name='unit-route')
    try:
        assert cc.server_id() == 'unit-server'
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


def test_relay_resolved_connect_delegates_route_validation_to_runtime_session(monkeypatch):
    registry = _ProcessRegistry.get()
    settings.relay_anchor_address = 'http://registry-relay.test'
    calls = []

    class FakeRuntimeSession:
        client_config_frozen = False
        server_id = None
        server_id_override = None
        server_ipc_overrides = None
        client_ipc_overrides = None

        def set_relay_anchor_address(self, relay_address):  # noqa: ANN001
            self.relay_anchor_address_override = relay_address

        def connect_via_relay(
            self,
            route_name: str,
            expected_crm_ns: str,
            expected_crm_name: str,
            expected_crm_ver: str,
            expected_abi_hash: str,
            expected_signature_hash: str,
        ):
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

        def lease_tracker(self):
            return None

    class FakeRelayAwareClient:
        def close(self):
            pass

    registry._runtime_session = FakeRuntimeSession()  # noqa: SLF001

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


def test_relay_resolved_connect_uses_native_session_not_python_relay_client():
    registry = _ProcessRegistry.get()
    settings.relay_anchor_address = 'http://registry-relay.test'

    with pytest.raises(Exception):
        registry.connect(IUnitConfigCRM, name='unit-route')

    source = inspect.getsource(type(registry))
    assert 'RustRelayAwareHttpClient(' not in source
    assert '_relay_control_client_for' not in source


def test_relay_connected_http_mode_uses_single_core_client_call_path():
    repo_root = Path(__file__).resolve().parents[4]
    runtime_source = (
        repo_root / 'sdk/python/native/src/runtime_session_ffi.rs'
    ).read_text(encoding='utf-8')
    client_source = (
        repo_root / 'sdk/python/native/src/core_ffi.rs'
    ).read_text(encoding='utf-8')

    assert 'Connect::RelayAware' in runtime_source
    assert 'self.connect_core(py, expected, Connect::RelayAware)' in runtime_source
    assert 'inner: Mutex<Option<Client>>' in client_source
    assert '.call_held(method_name, &request)' in client_source
    assert 'RelayAwareHttpClient' not in runtime_source
    assert 'release_http_client_from_global_pool' not in client_source


def test_relay_resolved_connect_maps_native_404_to_resource_not_found(monkeypatch):
    registry = _ProcessRegistry.get()
    settings.relay_anchor_address = 'http://registry-relay.test'

    class FakeRuntimeSession:
        client_config_frozen = False

        def set_relay_anchor_address(self, relay_address):  # noqa: ANN001
            pass

        def connect_via_relay(
            self,
            route_name: str,
            expected_crm_ns: str,
            expected_crm_name: str,
            expected_crm_ver: str,
            expected_abi_hash: str,
            expected_signature_hash: str,
        ):  # noqa: ARG002
            err = RuntimeError('HTTP 404')
            err.status_code = 404
            raise err

        def lease_tracker(self):
            return None

    registry._runtime_session = FakeRuntimeSession()  # noqa: SLF001

    with pytest.raises(ResourceNotFound, match="Resource 'unit-route' not found"):
        registry.connect(IUnitConfigCRM, name='unit-route')


def test_removed_low_level_server_parameters_are_rejected():
    with pytest.raises(TypeError, match='ipc_config'):
        Server(bind_address='ipc://unit_removed_ipc_config', ipc_config=object())

    with pytest.raises(TypeError, match='shm_threshold'):
        Server(bind_address='ipc://unit_removed_shm_threshold', shm_threshold=8192)


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
    monkeypatch.setenv('C2_IPC_POOL_SEGMENT_SIZE', 'not-a-number')

    from c_two._native import resolve_relay_use_proxy

    assert resolve_relay_use_proxy() is True


def test_relay_settings_do_not_resolve_ipc_env(monkeypatch):
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', 'http://127.0.0.1:8080')
    monkeypatch.setenv('C2_IPC_POOL_SEGMENT_SIZE', 'not-a-number')

    assert settings.relay_anchor_address == 'http://127.0.0.1:8080'


def test_relay_anchor_address_resolution_ignores_relay_proxy_policy(monkeypatch):
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', 'http://127.0.0.1:8080')
    monkeypatch.setenv('C2_RELAY_USE_PROXY', 'not-a-bool')

    assert settings.relay_anchor_address == 'http://127.0.0.1:8080'


def test_shm_threshold_resolution_does_not_parse_ipc_env(monkeypatch):
    monkeypatch.setenv('C2_SHM_THRESHOLD', '8192')
    monkeypatch.setenv('C2_IPC_POOL_SEGMENT_SIZE', 'not-a-number')

    assert settings.shm_threshold == 8192


def test_relay_anchor_env_replaces_old_relay_address_env(monkeypatch):
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', 'http://127.0.0.1:8080')
    monkeypatch.setenv('C2_RELAY_ADDRESS', 'http://old-relay.test:8080')

    assert settings.relay_anchor_address == 'http://127.0.0.1:8080'


def test_public_relay_anchor_api_replaces_set_relay():
    assert hasattr(cc, 'set_relay_anchor')
    assert not hasattr(cc, 'set_relay')
