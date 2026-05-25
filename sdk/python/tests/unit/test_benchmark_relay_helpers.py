from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
from pathlib import Path
from types import ModuleType

import pytest

from c_two.crm.contract import crm_contract


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[4]


def _load_benchmark(script_name: str) -> ModuleType:
    path = _repo_root() / 'sdk/python/benchmarks' / script_name
    spec = importlib.util.spec_from_file_location(script_name.removesuffix('.py'), path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def test_relay_qps_unregister_sends_server_id():
    module = _load_benchmark('relay_qps_benchmark.py')
    calls: list[tuple[str, dict[str, str]]] = []
    module._post_json = lambda url, payload: calls.append((url, payload))

    module._unregister_relay_upstream('http://127.0.0.1:8300', 'grid', 'srv-1')

    assert calls == [
        ('http://127.0.0.1:8300/_unregister', {'name': 'grid', 'server_id': 'srv-1'}),
    ]


def test_relay_qps_register_sends_instance_and_contract_fields():
    module = _load_benchmark('relay_qps_benchmark.py')
    calls: list[tuple[str, dict[str, object]]] = []
    module._post_json = lambda url, payload: calls.append((url, payload))
    expected = crm_contract(module.Hello)
    max_payload_size = 8192

    module._register_relay_upstream(
        'http://127.0.0.1:8300',
        'grid',
        'ipc://grid',
        'srv-1',
        'inst-1',
        expected,
        max_payload_size,
    )

    assert calls == [
        (
            'http://127.0.0.1:8300/_register',
            {
                'name': 'grid',
                'server_id': 'srv-1',
                'server_instance_id': 'inst-1',
                'address': 'ipc://grid',
                'crm_ns': expected.crm_ns,
                'crm_name': expected.crm_name,
                'crm_ver': expected.crm_ver,
                'abi_hash': expected.abi_hash,
                'signature_hash': expected.signature_hash,
                'max_payload_size': max_payload_size,
            },
        ),
    ]


def test_relay_qps_hey_args_include_expected_contract_headers():
    module = _load_benchmark('relay_qps_benchmark.py')
    expected = crm_contract(module.Hello)

    args = module._expected_contract_hey_args(expected)

    assert args == [
        '-H',
        f'x-c2-expected-crm-ns: {expected.crm_ns}',
        '-H',
        f'x-c2-expected-crm-name: {expected.crm_name}',
        '-H',
        f'x-c2-expected-crm-ver: {expected.crm_ver}',
        '-H',
        f'x-c2-expected-abi-hash: {expected.abi_hash}',
        '-H',
        f'x-c2-expected-signature-hash: {expected.signature_hash}',
    ]


def test_three_mode_unregister_sends_server_id():
    module = _load_benchmark('three_mode_benchmark.py')
    calls: list[tuple[str, dict[str, str]]] = []
    module._post_json = lambda url, payload: calls.append((url, payload))

    module._unregister_relay_upstream('http://127.0.0.1:8300', 'grid', 'srv-1')

    assert calls == [
        ('http://127.0.0.1:8300/_unregister', {'name': 'grid', 'server_id': 'srv-1'}),
    ]


def test_three_mode_register_sends_instance_and_contract_fields():
    module = _load_benchmark('three_mode_benchmark.py')
    calls: list[tuple[str, dict[str, object]]] = []
    module._post_json = lambda url, payload: calls.append((url, payload))
    expected = crm_contract(module.Echo)
    max_payload_size = 8192

    module._register_relay_upstream(
        'http://127.0.0.1:8300',
        'grid',
        'ipc://grid',
        'srv-1',
        'inst-1',
        expected,
        max_payload_size,
    )

    assert calls == [
        (
            'http://127.0.0.1:8300/_register',
            {
                'name': 'grid',
                'server_id': 'srv-1',
                'server_instance_id': 'inst-1',
                'address': 'ipc://grid',
                'crm_ns': expected.crm_ns,
                'crm_name': expected.crm_name,
                'crm_ver': expected.crm_ver,
                'abi_hash': expected.abi_hash,
                'signature_hash': expected.signature_hash,
                'max_payload_size': max_payload_size,
            },
        ),
    ]


def test_three_mode_relay_large_payload_rows_have_explicit_path_label():
    module = _load_benchmark('three_mode_benchmark.py')

    assert module.RELAY_TRANSPORT_LABEL == 'relay_http_request_stream_to_shm_response_materialized'
    assert module.RELAY_CLIENT_REQUEST_BODY_PATH == 'materialized'
    assert module.RELAY_RELAY_REQUEST_BODY_PATH == 'streamed_to_upstream_ipc'
    assert module.RELAY_RESPONSE_BODY_PATH == 'materialized'
    assert all(module._should_run_relay(size) for size, _label in module.SIZES)


def test_three_mode_relay_output_guardrails_are_source_visible():
    source = (_repo_root() / 'sdk/python/benchmarks/three_mode_benchmark.py').read_text()

    assert 'size_bytes <= 100 * 1024 * 1024' not in source
    assert 'relay_client_request_http' in source
    assert 'relay_request_to_ipc' in source
    assert 'relay_response_http' in source


def test_benchmark_results_are_local_only_artifacts():
    repo = _repo_root()
    if not (repo / '.git').exists():
        pytest.skip('git metadata unavailable')

    probe_path = 'sdk/python/benchmarks/results/probe.tsv'
    ignored = subprocess.run(
        ['git', 'check-ignore', '--no-index', probe_path],
        cwd=repo,
        text=True,
        capture_output=True,
    )

    assert ignored.returncode == 0, ignored.stderr

    tracked = subprocess.run(
        ['git', 'ls-files', 'sdk/python/benchmarks/results'],
        cwd=repo,
        text=True,
        capture_output=True,
        check=True,
    )
    tracked_paths = [
        line for line in tracked.stdout.splitlines()
        if line and (repo / line).exists()
    ]

    assert tracked_paths == ['sdk/python/benchmarks/results/.gitkeep']


def test_kostya_ctwo_fastdb_variant_uses_fdb_call_db_hold_contract():
    module = _load_benchmark('kostya_ctwo_benchmark.py')
    import c_two as cc

    descriptor = json.loads(cc.export_contract_descriptor(module.ICoordFastdb))
    method = next(item for item in descriptor['methods'] if item['name'] == 'coords')

    assert method['wire']['input']['id'] == 'org.fastdb.call-db'
    assert method['wire']['output']['id'] == 'org.fastdb.call-db'
    assert 'buffer-view' in method['wire']['output']['capabilities']
    assert module.VARIANTS['fastdb-hold'].use_hold is True


def test_kostya_ctwo_fastdb_normal_uses_owned_view_columnar_consumer():
    module = _load_benchmark('kostya_ctwo_benchmark.py')

    assert module.VARIANTS['fastdb-normal'].use_hold is False
    assert module.VARIANTS['fastdb-normal'].consumer is module.consume_fastdb_safe


def test_kostya_ctwo_benchmark_no_longer_uses_legacy_custom_fastdb_wrapper():
    source = (_repo_root() / 'sdk/python/benchmarks/kostya_ctwo_benchmark.py').read_text()

    assert 'class CoordFastdb:' not in source
    assert 'cc.hold(method)' in source


def test_three_mode_dict_echo_contract_is_descriptor_safe():
    module = _load_benchmark('three_mode_benchmark.py')

    contract = crm_contract(module.DictEcho)

    assert len(contract.abi_hash) == 64
    assert len(contract.signature_hash) == 64


def test_segment_size_dict_echo_contract_is_descriptor_safe():
    module = _load_benchmark('segment_size_benchmark.py')

    contract = crm_contract(module.DictEcho)

    assert len(contract.abi_hash) == 64
    assert len(contract.signature_hash) == 64


def test_relay_qps_best_effort_unregister_passes_server_id():
    module = _load_benchmark('relay_qps_benchmark.py')
    calls: list[tuple[str, str, str]] = []
    module._unregister_relay_upstream = lambda relay_url, name, server_id: calls.append(
        (relay_url, name, server_id)
    )

    module._best_effort_unregister_relay_upstream('http://127.0.0.1:8300', 'grid', 'srv-1')

    assert calls == [('http://127.0.0.1:8300', 'grid', 'srv-1')]


def test_three_mode_best_effort_unregister_passes_server_id():
    module = _load_benchmark('three_mode_benchmark.py')
    calls: list[tuple[str, str, str]] = []
    module._unregister_relay_upstream = lambda relay_url, name, server_id: calls.append(
        (relay_url, name, server_id)
    )

    module._best_effort_unregister_relay_upstream('http://127.0.0.1:8300', 'grid', 'srv-1')

    assert calls == [('http://127.0.0.1:8300', 'grid', 'srv-1')]
