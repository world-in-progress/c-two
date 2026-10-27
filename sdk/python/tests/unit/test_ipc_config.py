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
    cc.shutdown()
    _reset_default_runtime()
    settings.relay_anchor_address = None
    settings.shm_threshold = None
    settings.remote_payload_chunk_size = None
    monkeypatch.setenv('C2_ENV_FILE', '')
    for key in (
        'C2_SHM_THRESHOLD',
        'C2_REMOTE_PAYLOAD_CHUNK_SIZE',
        'C2_IPC_POOL_SEGMENT_SIZE',
        'C2_IPC_REASSEMBLY_SEGMENT_SIZE',
        'C2_IPC_POOL_PREWARM_SEGMENTS',
        'C2_IPC_POOL_MIN_RETAINED_SEGMENTS',
        'C2_IPC_POOL_DECAY_SECONDS',
        'C2_IPC_SHM_BACKING_BUDGET_BYTES',
        'C2_IPC_FILE_BACKING_BUDGET_BYTES',
        'C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES',
        'C2_RELAY_ANCHOR_ADDRESS',
        'C2_RELAY_USE_PROXY',
        'C2_RELAY_ROUTE_MAX_ATTEMPTS',
    ):
        monkeypatch.delenv(key, raising=False)
    yield
    try:
        cc.shutdown()
        _reset_default_runtime()
    finally:
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
    assert base['pool_prewarm_segments'] is int
    assert base['pool_min_retained_segments'] is int
    assert base['chunk_gc_interval'] is float
    assert base['shm_backing_budget_bytes'] is int
    assert base['file_backing_budget_bytes'] is int
    assert base['live_reassembly_budget_bytes'] is int
    assert server['heartbeat_interval'] is float
    assert server['max_pending_requests'] is int
    assert server['max_execution_workers'] is int
    assert server['pool_decay_seconds'] is float
    assert client['reassembly_segment_size'] is int
    assert client['pool_decay_seconds'] is float

    for hints in (base, server, client):
        assert 'shm_threshold' not in hints
        assert 'max_pool_memory' not in hints


def test_server_config_uses_env_for_ipc_defaults(monkeypatch):
    monkeypatch.setenv('C2_IPC_POOL_SEGMENT_SIZE', str(2 * 1024 * 1024))

    server_config = ipc_config._resolve_server_ipc_config()
    assert server_config['pool_segment_size'] == 2 * 1024 * 1024


def test_server_config_uses_env_for_global_transport_policy(monkeypatch):
    monkeypatch.setenv('C2_SHM_THRESHOLD', '8192')

    server_config = ipc_config._resolve_server_ipc_config()
    assert server_config['shm_threshold'] == 8192


def test_transport_policy_override_beats_env(monkeypatch):
    monkeypatch.setenv('C2_SHM_THRESHOLD', '8192')

    cc.set_transport_policy(shm_threshold=16384)

    server_config = ipc_config._resolve_server_ipc_config()
    assert server_config['shm_threshold'] == 16384


def test_remote_payload_chunk_size_resolves_from_env_and_policy(monkeypatch):
    monkeypatch.setenv('C2_REMOTE_PAYLOAD_CHUNK_SIZE', '2097152')

    from c_two._native import resolve_remote_payload_chunk_size

    assert resolve_remote_payload_chunk_size() == 2_097_152
    assert settings.remote_payload_chunk_size == 2_097_152

    cc.set_transport_policy(remote_payload_chunk_size=1_048_576)
    registry = _get_default_runtime()

    assert settings.remote_payload_chunk_size == 1_048_576
    assert registry._runtime_session.remote_payload_chunk_size_override == 1_048_576  # noqa: SLF001


def test_transport_policy_updates_preserve_unspecified_overrides():
    cc.set_transport_policy(shm_threshold=8192)
    cc.set_transport_policy(remote_payload_chunk_size=1_048_576)
    registry = _get_default_runtime()

    assert settings.shm_threshold == 8192
    assert settings.remote_payload_chunk_size == 1_048_576
    assert registry._runtime_session.client_ipc_config['shm_threshold'] == 8192  # noqa: SLF001
    assert registry._runtime_session.remote_payload_chunk_size_override == 1_048_576  # noqa: SLF001

    cc.set_transport_policy(shm_threshold=None)
    registry = _get_default_runtime()

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

    server_config = ipc_config._resolve_server_ipc_config({'pool_segment_size': 4 * 1024 * 1024})
    assert server_config['pool_segment_size'] == 4 * 1024 * 1024


def test_server_execution_workers_env_and_override(monkeypatch):
    monkeypatch.setenv('C2_IPC_MAX_EXECUTION_WORKERS', '9')

    server_config = ipc_config._resolve_server_ipc_config()
    assert server_config['max_execution_workers'] == 9

    override_server_config = ipc_config._resolve_server_ipc_config({'max_execution_workers': 7})
    assert override_server_config['max_execution_workers'] == 7


def test_server_execution_workers_zero_rejected(monkeypatch):
    monkeypatch.setenv('C2_IPC_MAX_EXECUTION_WORKERS', '0')

    with pytest.raises(ValueError, match='max_execution_workers'):
        ipc_config._resolve_server_ipc_config()

    monkeypatch.delenv('C2_IPC_MAX_EXECUTION_WORKERS')
    with pytest.raises(ValueError, match='max_execution_workers'):
        ipc_config._resolve_server_ipc_config({'max_execution_workers': 0})


def test_server_execution_workers_above_hard_limit_rejected(monkeypatch):
    monkeypatch.setenv('C2_IPC_MAX_EXECUTION_WORKERS', '65')

    with pytest.raises(ValueError, match='max_execution_workers'):
        ipc_config._resolve_server_ipc_config()

    monkeypatch.delenv('C2_IPC_MAX_EXECUTION_WORKERS')
    with pytest.raises(ValueError, match='max_execution_workers'):
        ipc_config._resolve_server_ipc_config({'max_execution_workers': 65})


GIB = 1024 ** 3
U64_MAX = 2 ** 64 - 1


def test_resolved_ipc_config_exposes_canonical_memory_budget_defaults():
    native = ipc_config._native_resolver()  # noqa: SLF001

    server_cfg = native.resolve_server_ipc_config(None, None)
    client_cfg = native.resolve_client_ipc_config(None, None)

    for cfg in (server_cfg, client_cfg):
        assert cfg['shm_backing_budget_bytes'] == 8 * GIB
        assert cfg['file_backing_budget_bytes'] == 16 * GIB
        assert cfg['live_reassembly_budget_bytes'] == 8 * GIB


@pytest.mark.parametrize(
    'budgets',
    [(3 * GIB, 5 * GIB, GIB), (0, 0, 0), (U64_MAX, U64_MAX, U64_MAX)],
    ids=['explicit', 'zero', 'u64-max'],
)
def test_ipc_overrides_roundtrip_memory_budget_bytes(budgets):
    native = ipc_config._native_resolver()  # noqa: SLF001
    overrides = {
        'shm_backing_budget_bytes': budgets[0],
        'file_backing_budget_bytes': budgets[1],
        'live_reassembly_budget_bytes': budgets[2],
    }

    server_cfg = native.resolve_server_ipc_config(overrides, None)
    client_cfg = native.resolve_client_ipc_config(overrides, None)

    for cfg in (server_cfg, client_cfg):
        assert {key: cfg[key] for key in overrides} == overrides


_COMMON_IPC_FIELDS = (
    ('pool_enabled', 'C2_IPC_POOL_ENABLED', True, False),
    ('pool_segment_size', 'C2_IPC_POOL_SEGMENT_SIZE', 131072, 262144),
    ('max_pool_segments', 'C2_IPC_MAX_POOL_SEGMENTS', 2, 3),
    ('pool_prewarm_segments', 'C2_IPC_POOL_PREWARM_SEGMENTS', 1, 0),
    ('pool_min_retained_segments', 'C2_IPC_POOL_MIN_RETAINED_SEGMENTS', 2, 0),
    ('reassembly_segment_size', 'C2_IPC_REASSEMBLY_SEGMENT_SIZE', 65536, 131072),
    ('reassembly_max_segments', 'C2_IPC_REASSEMBLY_MAX_SEGMENTS', 3, 2),
    ('max_total_chunks', 'C2_IPC_MAX_TOTAL_CHUNKS', 8192, 16384),
    ('chunk_gc_interval', 'C2_IPC_CHUNK_GC_INTERVAL', 2.5, 5.0),
    ('chunk_threshold_ratio', 'C2_IPC_CHUNK_THRESHOLD_RATIO', 0.5, 0.75),
    ('chunk_assembler_timeout', 'C2_IPC_CHUNK_ASSEMBLER_TIMEOUT', 30.0, 60.0),
    ('max_reassembly_bytes', 'C2_IPC_MAX_REASSEMBLY_BYTES', 1048576, 2097152),
    ('chunk_size', 'C2_IPC_CHUNK_SIZE', 65536, 131072),
    ('shm_backing_budget_bytes', 'C2_IPC_SHM_BACKING_BUDGET_BYTES', GIB, 0),
    ('file_backing_budget_bytes', 'C2_IPC_FILE_BACKING_BUDGET_BYTES', 2 * GIB, 0),
    ('live_reassembly_budget_bytes', 'C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES', 4 * GIB, 0),
    ('pool_decay_seconds', 'C2_IPC_POOL_DECAY_SECONDS', 12.5, 7.5),
)
_SERVER_IPC_FIELDS = (
    ('max_frame_size', 'C2_IPC_MAX_FRAME_SIZE', 1048576, 2097152),
    ('max_payload_size', 'C2_IPC_MAX_PAYLOAD_SIZE', 1048576, 2097152),
    ('max_pending_requests', 'C2_IPC_MAX_PENDING_REQUESTS', 7, 11),
    ('max_execution_workers', 'C2_IPC_MAX_EXECUTION_WORKERS', 9, 7),
    ('heartbeat_interval', 'C2_IPC_HEARTBEAT_INTERVAL', 3.0, 0.0),
    ('heartbeat_timeout', 'C2_IPC_HEARTBEAT_TIMEOUT', 20.0, 45.0),
)


@pytest.mark.parametrize(
    'role,field,key,env_value,override_value',
    [(role, *case) for role in ('server', 'client') for case in _COMMON_IPC_FIELDS]
    + [('server', *case) for case in _SERVER_IPC_FIELDS],
)
def test_ipc_field_precedence_and_overridden_invalid_env(
    monkeypatch, role, field, key, env_value, override_value,
):
    resolve = getattr(ipc_config._native_resolver(), f'resolve_{role}_ipc_config')
    monkeypatch.setenv(key, str(env_value))
    assert resolve(None, None)[field] == env_value
    assert resolve({field: override_value}, None)[field] == override_value

    monkeypatch.setenv(key, 'invalid')
    for overrides in (None, {field: override_value}):
        with pytest.raises(ValueError, match=key):
            resolve(overrides, None)


@pytest.mark.parametrize('role', ('server', 'client'))
def test_ipc_env_errors_precede_derivation_and_final_validation(monkeypatch, role):
    resolve = getattr(ipc_config._native_resolver(), f'resolve_{role}_ipc_config')
    overrides = {'pool_segment_size': 2 ** 64 - 1}
    monkeypatch.setenv('C2_IPC_POOL_SEGMENT_SIZE', 'invalid')
    monkeypatch.setenv('C2_IPC_POOL_DECAY_SECONDS', 'invalid')
    with pytest.raises(ValueError, match='C2_IPC_POOL_SEGMENT_SIZE'):
        resolve(overrides, None)
    monkeypatch.delenv('C2_IPC_POOL_SEGMENT_SIZE')
    with pytest.raises(ValueError, match='C2_IPC_POOL_DECAY_SECONDS'):
        resolve(overrides, None)
    monkeypatch.delenv('C2_IPC_POOL_DECAY_SECONDS')
    with pytest.raises(ValueError, match='max_pool_memory derived value overflowed'):
        resolve(overrides, None)
    with pytest.raises(ValueError, match='pool_segment_size must be > 0'):
        resolve({'pool_segment_size': 0}, None)


def test_memory_budget_negative_and_overflow_overrides_rejected_by_native():
    native = ipc_config._native_resolver()  # noqa: SLF001

    for bad_value in (-1, 2 ** 64):
        with pytest.raises(OverflowError):
            native.resolve_client_ipc_config(
                {'shm_backing_budget_bytes': bad_value}, None
            )
        with pytest.raises(OverflowError):
            native.resolve_server_ipc_config(
                {'live_reassembly_budget_bytes': bad_value}, None
            )


def test_memory_budget_invalid_env_value_rejected_by_native(monkeypatch):
    native = ipc_config._native_resolver()  # noqa: SLF001

    for key, bad_value in (
        ('C2_IPC_SHM_BACKING_BUDGET_BYTES', '-1'),
        ('C2_IPC_FILE_BACKING_BUDGET_BYTES', str(2 ** 64)),
        ('C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES', 'not-a-number'),
    ):
        monkeypatch.setenv(key, bad_value)
        with pytest.raises(ValueError, match=key):
            native.resolve_server_ipc_config(None, None)
        with pytest.raises(ValueError, match=key):
            native.resolve_client_ipc_config(None, None)
        monkeypatch.delenv(key)


def test_client_session_projects_memory_budget_overrides():
    cc.set_client(ipc_overrides={'shm_backing_budget_bytes': 2 * GIB})
    registry = _get_default_runtime()

    client_config = registry._runtime_session.client_ipc_config  # noqa: SLF001
    assert client_config['shm_backing_budget_bytes'] == 2 * GIB
    assert client_config['file_backing_budget_bytes'] == 16 * GIB


def test_server_config_projects_memory_budget_env(monkeypatch):
    monkeypatch.setenv('C2_IPC_FILE_BACKING_BUDGET_BYTES', str(32 * GIB))

    server_config = ipc_config._resolve_server_ipc_config()
    assert server_config['file_backing_budget_bytes'] == 32 * GIB
    assert server_config['shm_backing_budget_bytes'] == 8 * GIB


def test_client_ipc_overrides_beat_env(monkeypatch):
    monkeypatch.setenv('C2_IPC_REASSEMBLY_SEGMENT_SIZE', str(32 * 1024 * 1024))

    cc.set_client(ipc_overrides={'reassembly_segment_size': 16 * 1024 * 1024})
    registry = _get_default_runtime()

    assert registry._runtime_session.client_ipc_config['reassembly_segment_size'] == 16 * 1024 * 1024  # noqa: SLF001


def test_client_pool_decay_defaults_and_projects_into_resolved_config():
    native = ipc_config._native_resolver()  # noqa: SLF001

    client_cfg = native.resolve_client_ipc_config(None, None)
    assert client_cfg['pool_decay_seconds'] == 60.0

    client_cfg = native.resolve_client_ipc_config({'pool_decay_seconds': 2.5}, None)
    assert client_cfg['pool_decay_seconds'] == 2.5


def test_client_pool_decay_zero_is_immediate_retirement_and_negative_is_rejected():
    native = ipc_config._native_resolver()  # noqa: SLF001

    client_cfg = native.resolve_client_ipc_config({'pool_decay_seconds': 0.0}, None)
    assert client_cfg['pool_decay_seconds'] == 0.0

    with pytest.raises(ValueError, match='pool_decay_seconds'):
        native.resolve_client_ipc_config({'pool_decay_seconds': -1.0}, None)


def test_buddy_policy_defaults_are_lazy_and_retire_to_zero():
    native = ipc_config._native_resolver()  # noqa: SLF001

    server_cfg = native.resolve_server_ipc_config(None, None)
    client_cfg = native.resolve_client_ipc_config(None, None)

    for cfg in (server_cfg, client_cfg):
        assert cfg['pool_enabled'] is True
        assert cfg['pool_prewarm_segments'] == 0
        assert cfg['pool_min_retained_segments'] == 0


def test_buddy_policy_overrides_beat_env(monkeypatch):
    monkeypatch.setenv('C2_IPC_POOL_PREWARM_SEGMENTS', '1')
    monkeypatch.setenv('C2_IPC_POOL_MIN_RETAINED_SEGMENTS', '1')

    server_config = ipc_config._resolve_server_ipc_config({'pool_prewarm_segments': 2, 'pool_min_retained_segments': 3})
    assert server_config['pool_prewarm_segments'] == 2
    assert server_config['pool_min_retained_segments'] == 3

    native = ipc_config._native_resolver()
    client_cfg = native.resolve_client_ipc_config(
        {'pool_prewarm_segments': 0, 'pool_min_retained_segments': 2}, None
    )
    assert client_cfg['pool_prewarm_segments'] == 0
    assert client_cfg['pool_min_retained_segments'] == 2


def test_prewarm_with_buddy_disabled_is_rejected_by_native():
    native = ipc_config._native_resolver()  # noqa: SLF001

    with pytest.raises(ValueError, match='pool_prewarm_segments'):
        native.resolve_server_ipc_config(
            {'pool_enabled': False, 'pool_prewarm_segments': 1}, None
        )

    with pytest.raises(ValueError, match='pool_prewarm_segments'):
        native.resolve_client_ipc_config(
            {'pool_enabled': False, 'pool_prewarm_segments': 1}, None
        )


def test_prewarm_exceeding_max_pool_segments_is_rejected_by_native():
    native = ipc_config._native_resolver()  # noqa: SLF001

    with pytest.raises(ValueError, match='pool_prewarm_segments'):
        native.resolve_server_ipc_config(
            {'max_pool_segments': 2, 'pool_prewarm_segments': 3}, None
        )


def test_min_retained_exceeding_limits_is_rejected_by_native():
    native = ipc_config._native_resolver()  # noqa: SLF001

    with pytest.raises(ValueError, match='pool_min_retained_segments'):
        native.resolve_client_ipc_config(
            {'max_pool_segments': 2, 'pool_min_retained_segments': 3}, None
        )

    with pytest.raises(ValueError, match='reassembly_max_segments'):
        native.resolve_client_ipc_config(
            {'reassembly_max_segments': 2, 'pool_min_retained_segments': 3}, None
        )


@pytest.mark.parametrize('key', ['shm_threshold'])
def test_forbidden_server_ipc_override_fields_are_rejected_by_native(key):
    with pytest.raises(ValueError, match='shm_threshold is a global transport policy'):
        cc.set_server(ipc_overrides={key: 1})


@pytest.mark.parametrize('key', ['shm_threshold'])
def test_forbidden_client_ipc_override_fields_are_rejected_by_native(key):
    with pytest.raises(ValueError, match='shm_threshold is a global transport policy'):
        cc.set_client(ipc_overrides={key: 1})


def test_removed_max_pool_memory_override_is_rejected_by_native():
    with pytest.raises(ValueError, match='unknown IPC override option: max_pool_memory'):
        cc.set_server(ipc_overrides={'max_pool_memory': 1024})

    with pytest.raises(ValueError, match='unknown IPC override option: max_pool_memory'):
        cc.set_client(ipc_overrides={'max_pool_memory': 1024})


def test_native_resolver_treats_max_pool_memory_as_unknown_override():
    native = ipc_config._native_resolver()  # noqa: SLF001

    with pytest.raises(ValueError, match='unknown IPC override option: max_pool_memory'):
        native.resolve_server_ipc_config({'max_pool_memory': 1024}, None)

    with pytest.raises(ValueError, match='unknown IPC override option: max_pool_memory'):
        native.resolve_client_ipc_config({'max_pool_memory': 1024}, None)


@pytest.mark.parametrize('segment_size, segments, total', (
    (268_435_456, 4, 1_073_741_824),
    (131_072, 3, 393_216),
    (524_288, 5, 2_621_440),
))
def test_native_resolver_accepts_mapping_subclasses(segment_size, segments, total):
    native = ipc_config._native_resolver()  # noqa: SLF001
    overrides = UserDict({'pool_segment_size': segment_size, 'max_pool_segments': segments})

    server_cfg = native.resolve_server_ipc_config(overrides, None)
    client_cfg = native.resolve_client_ipc_config(overrides, None)

    for cfg in (server_cfg, client_cfg):
        assert cfg['pool_segment_size'] == segment_size
        assert cfg['max_pool_segments'] == segments
        assert cfg['max_pool_memory'] == total


def test_unknown_ipc_override_field_is_rejected_by_native():
    with pytest.raises(ValueError, match='unknown IPC override option: not_real'):
        ipc_config._resolve_server_ipc_config({'not_real': 1})

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

    registry = _get_default_runtime()
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

    registry = _get_default_runtime()
    assert registry._runtime_session.server_ipc_overrides == {
        'pool_segment_size': 2 * 1024 * 1024,
    }  # noqa: SLF001




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
    server_ipc_overrides = None
    client_ipc_overrides = None

    def __init__(
        self,
        *,
        max_outstanding_calls: int | None = None,
        retained_input_budget_bytes: int | None = None,
        **_kwargs,
    ) -> None:
        from c_two._native import resolve_local_endpoint_context

        self._context = resolve_local_endpoint_context()
        self._limits = {
            key: value for key, value in (
                ('max_outstanding_calls', max_outstanding_calls),
                ('retained_input_budget_bytes', retained_input_budget_bytes),
            ) if value is not None
        }
        self.adopted_observations: list[_FakeRetiredObservation] = []

    @property
    def call_execution_limits_overrides(self) -> dict[str, int]:
        return dict(self._limits)

    def local_endpoint_context(self) -> cc.LocalEndpointContext:
        return self._context

    def inherit_local_endpoint_selection(self, previous: _FakeRuntimeSession) -> None:
        self._context = previous._context

    def shutdown(self, *, route_names, relay_anchor_address, timeout_seconds):
        assert route_names == []
        return {'completed': True, 'relay_errors': [], 'route_outcomes': []}

    def retire_memory_observation(self) -> _FakeRetiredObservation:
        return _FakeRetiredObservation()

    def adopt_retired_memory_observation(self, observation: _FakeRetiredObservation) -> None:
        self.adopted_observations.append(observation)

    def set_relay_anchor_address(self, relay_address: str | None, *, connect_attempt=None) -> None:
        self.relay_anchor_address_override = relay_address
        self.connect_attempt = connect_attempt

    def lease_tracker(self):
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
        max_outstanding_calls=7, retained_input_budget_bytes=8192,
    )
    captured = installed.local_endpoint_context()

    with monkeypatch.context() as patch:
        patch.setattr(registry, '_runtime_session', installed)
        assert registry.shutdown()['completed'] is True
        replacement = registry._runtime_session  # noqa: SLF001
        assert replacement is not installed
        assert replacement.local_endpoint_context() is captured
        assert replacement.call_execution_limits_overrides == {
            'max_outstanding_calls': 7, 'retained_input_budget_bytes': 8192,
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
    # Encoding consumes the prepared scope; execution receives only that
    # encoded owner, with no second route or method argument.
    assert re.search(r'fn\s+finish\s*\([^)]*encoded:\s*c2_core::EncodedCall\s*,?\s*\)', client_source)
    assert re.search(r'py\s*\.detach\(move\s*\|\|\s*encoded\.call_held\(\)\)', client_source)
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
