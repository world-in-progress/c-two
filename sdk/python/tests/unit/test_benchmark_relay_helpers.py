from __future__ import annotations

import importlib.util
import subprocess
import sys
import threading
from concurrent.futures import ThreadPoolExecutor
from contextlib import nullcontext
from pathlib import Path
from types import ModuleType

import pytest
import httpx

from c_two.crm.contract import crm_contract


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[4]


@pytest.fixture(autouse=True)
def _benchmark_import_path(monkeypatch):
    monkeypatch.syspath_prepend(str(_repo_root() / 'sdk/python/benchmarks'))


def _load_benchmark(script_name: str) -> ModuleType:
    path = _repo_root() / 'sdk/python/benchmarks' / script_name
    spec = importlib.util.spec_from_file_location(script_name.removesuffix('.py'), path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def test_relay_session_restores_unset_anchor_after_error(monkeypatch, capsys):
    import _relay_support as relay

    original = relay.settings._relay_anchor_address
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', 'http://environment:8300')
    monkeypatch.setattr(relay._NO_PROXY_OPENER, 'open', lambda *a, **kw: nullcontext())
    monkeypatch.setattr(relay.cc, 'shutdown', lambda: {
        'completed': True, 'relay_errors': ['withdrawal failed'],
    })
    relay.cc.set_relay_anchor('')
    try:
        with pytest.raises(ValueError, match='benchmark'):
            with relay.session('http://127.0.0.1:8300'):
                raise ValueError('benchmark')
        assert relay.settings._relay_anchor_address is None
        assert relay.settings.relay_anchor_address == 'http://environment:8300'
        assert 'withdrawal failed' in capsys.readouterr().err
    finally:
        relay.cc.set_relay_anchor(original or '')


def test_relay_qps_hey_args_include_contract_and_route_identity():
    module = _load_benchmark('relay_qps_benchmark.py')
    expected = crm_contract(module.Hello)

    args = module._route_hey_args(expected, 'registered-route-uid', 7)

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
        '-H',
        'x-c2-route-uid: registered-route-uid',
        '-H',
        'x-c2-route-revision: 7',
    ]


@pytest.mark.parametrize('script', ['relay_qps_benchmark.py', 'three_mode_benchmark.py'])
def test_benchmark_http_calls_and_cleanup(script, start_c3_relay, monkeypatch, tmp_path):
    relay_process = start_c3_relay()
    port = int(relay_process.bind.rsplit(':', 1)[1])
    module = _load_benchmark(script)
    settings = module.relay.settings
    original_anchor = settings._relay_anchor_address
    connect = module.cc.connect

    def connect_http(*args, **kwargs):
        proxy = connect(*args, **kwargs)
        assert proxy.client.observed_path == 'explicit_relay'
        return proxy

    monkeypatch.setattr(module.cc, 'connect', connect_http)
    module.cc.set_relay_anchor('http://previous:8300')
    try:
        if script == 'relay_qps_benchmark.py':
            monkeypatch.setattr(module, 'RELAY_PORT', port)
            monkeypatch.setattr(module, 'TOTAL_REQUESTS', 3)
            monkeypatch.setattr(module, 'CONCURRENCY', 1)
            payload = tmp_path / 'hey-payload.bin'
            monkeypatch.setattr(module, 'PAYLOAD_FILE', str(payload))
            payload.write_bytes(b'previous payload')
            def unavailable(*args, **kwargs):
                raise OSError('offline')
            with monkeypatch.context() as offline:
                offline.setattr(module.relay._NO_PROXY_OPENER, 'open', unavailable)
                with pytest.raises(SystemExit, match='Start it first'):
                    module.main()
            assert payload.read_bytes() == b'previous payload'
            module.main()
            assert not payload.exists()
        else:
            monkeypatch.setattr(module, '_relay_port', port)
            monkeypatch.setattr(module, '_SEGMENT_SIZE', 1024 * 1024)
            monkeypatch.setattr(module, '_rounds', lambda _: 2)
            monkeypatch.setattr(module, 'WARMUP', 1)
            assert module.bench_relay(64) is not None
        assert settings._relay_anchor_address == 'http://previous:8300'
        with httpx.Client(trust_env=False) as http:
            assert http.get(f'{relay_process.url}/_routes').json()['routes'] == []
    finally:
        module.cc.shutdown()
        module.cc.set_relay_anchor(original_anchor or '')
        module._ProcessRegistry.reset()


@pytest.mark.parametrize('body_fails', [False, True])
def test_relay_session_keeps_anchor_while_native_call_drains(body_fails, start_c3_relay, monkeypatch, capsys):
    module = _load_benchmark('three_mode_benchmark.py')
    relay_process = start_c3_relay()
    original = module.relay.settings._relay_anchor_address
    started, finish = threading.Event(), threading.Event()
    shutdown = module.cc.shutdown
    monkeypatch.setattr(module.cc, 'shutdown', lambda: shutdown(timeout=0.01))

    class BlockingEcho:
        def echo(self, data):
            started.set()
            assert finish.wait(5)
            return data

    with ThreadPoolExecutor(max_workers=1) as pool:
        try:
            error = ValueError if body_fails else RuntimeError
            with pytest.raises(error, match='benchmark'):
                with module.relay.session(relay_process.url):
                    module.cc.register(module.Echo, BlockingEcho(), name='draining-echo')
                    with module.cc.connect(module.Echo, name='draining-echo') as proxy:
                        call = pool.submit(proxy.echo, b'pending')
                        assert started.wait(2)
                        if body_fails:
                            raise ValueError('benchmark')
            assert module.relay.settings._relay_anchor_address == relay_process.url
            if body_fails:
                assert 'anchor retained' in capsys.readouterr().err
            finish.set()
            assert call.result(timeout=2) == b'pending'
        finally:
            finish.set()
            assert shutdown()['completed']
            module.cc.set_relay_anchor(original or '')
            module._ProcessRegistry.reset()


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


def test_kostya_ctwo_benchmark_retains_only_truthful_pickle_baselines():
    module = _load_benchmark('kostya_ctwo_benchmark.py')

    assert set(module.VARIANTS) == {'pickle-records', 'pickle-arrays'}


def test_kostya_ctwo_benchmark_contains_no_deleted_fastdb_runtime_authority():
    source = (_repo_root() / 'sdk/python/benchmarks/kostya_ctwo_benchmark.py').read_text()

    assert 'org.fastdb.call-db' not in source
    assert 'call_db' not in source
    assert 'fastdb4py as fdb' not in source
    assert 'fdb.Batch' not in source


@pytest.mark.parametrize('script', ['three_mode_benchmark.py', 'segment_size_benchmark.py'])
def test_dict_echo_contract_is_descriptor_safe(script):
    module = _load_benchmark(script)
    contract = crm_contract(module.DictEcho)
    assert len(contract.abi_hash) == 64
    assert len(contract.signature_hash) == 64
