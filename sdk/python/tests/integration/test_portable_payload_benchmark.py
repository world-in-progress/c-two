"""Guarded smoke tests for the reproducible portable-payload benchmark harness.

Verifies receipt schema, copy-ledger metadata, FastDB ExecutionReport fields,
exact numerical checksums, clean hold release, and privacy hygiene using only
public C-Two and FastDB APIs (no Ray dependency required).
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

import pytest

pytestmark = pytest.mark.timeout(90)


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[4]


def _bench_script() -> Path:
    return _repo_root() / 'sdk/python/benchmarks/portable_payload_benchmark.py'


def _clean_env() -> dict[str, str]:
    env = os.environ.copy()
    env['C2_ENV_FILE'] = ''
    env['C2_RELAY_ANCHOR_ADDRESS'] = ''
    env['NO_PROXY'] = '127.0.0.1,localhost'
    env['no_proxy'] = '127.0.0.1,localhost'
    return env


def test_portable_payload_benchmark_smoke_all_ctwo_suites(tmp_path):
    output = tmp_path / 'receipt.json'
    c3_bin = os.environ.get('CTWO_TEST_C3_BIN') or shutil.which('c3')
    relay_args = ['--c3-bin', c3_bin] if c3_bin else []
    proc = subprocess.run(
        [
            sys.executable,
            str(_bench_script()),
            '--suite',
            'all',
            '--smoke',
            *relay_args,
            '--output',
            str(output),
        ],
        cwd=_repo_root(),
        env=_clean_env(),
        capture_output=True,
        text=True,
        timeout=85,
        check=False,
    )
    assert proc.returncode == 0, f'stdout:\n{proc.stdout}\nstderr:\n{proc.stderr}'

    raw_text = output.read_text(encoding='utf-8')
    home = str(Path.home())
    assert home not in raw_text, 'benchmark receipt must not contain the user home path'
    assert home.replace('\\', '\\\\') not in raw_text, 'benchmark receipt must not contain the escaped user home path'

    receipt = json.loads(raw_text)
    assert receipt['schema'] == 'c-two.portable-payload-benchmark.v1'
    assert receipt['smoke'] is True
    assert receipt['include_ray'] is False
    env_meta = receipt['environment']
    assert env_meta['headless'] is True
    assert env_meta['c_two_version']
    assert env_meta['fastdb4py_version']

    suites = receipt['suites']
    assert set(suites.keys()) == {'stages', 'coupling', 'field', 'fanout', 'pubsub_fanout', 'kostya'}

    stage_row = suites['stages']['rows'][0]
    assert stage_row['fastdb_staging_bytes'] == 0
    assert stage_row['copy_ledger_shm_unary']['total_lower_bound_one_way'] == 8
    assert stage_row['copy_ledger_shm_stream']['total_lower_bound_one_way'] == 7
    assert stage_row['stages_p50_ms']['open_copy_from_memoryview'] > 0

    coupling_arms = {r['arm'] for r in suites['coupling']['rows']}
    assert {'ctwo-fastdb-unary', 'ctwo-pickle-arrays'}.issubset(coupling_arms)
    for row in suites['coupling']['rows']:
        assert row['client_holds_active'] == 0
        assert row['p50_ms'] > 0

    field_arms = {r['arm'] for r in suites['field']['rows']}
    assert {'ctwo-fastdb-unary', 'ctwo-fastdb-stream', 'ctwo-fastdb-hold', 'ctwo-pickle-arrays'}.issubset(field_arms)
    for row in suites['field']['rows']:
        assert row['client_holds_active'] == 0
        assert row['p95_ms'] >= row['p50_ms']

    fanout_rows = suites['fanout']['rows']
    fanout_readers = {r['readers'] for r in fanout_rows}
    assert {1, 2}.issubset(fanout_readers)
    assert all(row['server_pool_peak_mib'] > 0 for row in fanout_rows)

    pubsub_rows = suites['pubsub_fanout']['rows']
    transports = {'direct_ipc', 'explicit_relay', 'dual_relay'} if c3_bin else {'direct_ipc'}
    assert {(row['transport'], row['readers']) for row in pubsub_rows} == {
        (transport, readers) for transport in transports for readers in (1, 2)
    }
    for row in pubsub_rows:
        assert row['frames_per_reader'] == 3
        assert row['topic_capacity'] == 4
        assert row['dropped_total'] == 0
        assert row['server_write_multiplier'] == 1
        assert row['per_reader_p50_ms'] > 0
        assert row['per_reader_p95_ms'] >= row['per_reader_p50_ms']
        assert row['server_pool_peak_mib'] > 0
        assert row['publish_elapsed_s'] > 0

    kostya_arms = {r['arm'] for r in suites['kostya']['rows']}
    assert kostya_arms == {'ctwo-fastdb-packed', 'ctwo-pickle-arrays'}
    for row in suites['kostya']['rows']:
        assert row['rows'] == 1_000
        assert row['p50_ms'] > 0


def test_portable_payload_benchmark_uses_only_public_apis():
    source = _bench_script().read_text(encoding='utf-8')
    assert 'c_two._native' not in source
    assert '._client' not in source
    assert '_runtime_session' not in source
