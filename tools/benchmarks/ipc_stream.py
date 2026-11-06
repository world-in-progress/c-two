"""Candidate-only unary/batch/server-stream IPC comparison.

This has its own evidence schema. The historical unary matrix and its paired
receipts remain inputs to ipc_paired.py, not inputs to this benchmark.
"""
from __future__ import annotations

import argparse
from collections.abc import Iterator
import hashlib
import json
import math
import os
from pathlib import Path
import pickle
import platform
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ipc_memory as bench
from ipc_evidence import (
    BenchmarkError, CORRECTNESS_FAILURE, Failures, SampleLog, TOOL_FAILURE,
    SCENARIO_NOT_ESTABLISHED, failure_message, process_snapshot, publish_marker, read_marker, run_owned,
)

ROOT = Path(__file__).resolve().parents[2]
MODES = ('U', 'Q', 'S')
MIB = 1 << 20
FRAME_BYTES = 64 * 1024
THRESHOLD = 4096
SCHEMA = 'c-two.ipc-stream-benchmark.v1'


def matrix_rows() -> list[dict]:
    rows = []
    for name, size, count, workers in (
        ('small', 1, 200, 1), ('burst', 64 * 1024, 40, 4),
        ('science', 4 * MIB, 8, 1), ('file_fallback', 2 * MIB, 2, 1),
    ):
        for buddy in (True, False):
            rows.append(dict(name=f'{name}_{"buddy" if buddy else "no_buddy"}',
                             size=size, count=count, workers=workers, buddy=buddy,
                             no_shm=name == 'file_fallback'))
    rows.append(dict(name='all_backing_zero', size=2 * MIB, count=1, no_shm=True,
                     zero_backing=True))
    for name, target, no_shm in (
        ('threshold_below', THRESHOLD - 1, False),
        ('threshold_at', THRESHOLD, False), ('threshold_above', THRESHOLD + 1, False),
        ('frame_below', FRAME_BYTES - 1, True), ('frame_at', FRAME_BYTES, True),
        ('frame_above', FRAME_BYTES + 1, True), ('four_frames', 4 * FRAME_BYTES, True),
    ):
        rows.append(dict(name=name, encoded_target=target, count=16, no_shm=no_shm))
    for offset in (-1, 0, 1):
        rows.append(dict(name=f'budget_{offset:+d}', size=64 * 1024, count=1,
                         budget_offset=offset))
    rows.extend([
        dict(name='empty_items', size=0, count=16), dict(name='empty_stream', size=0, count=0),
        dict(name='fastdb_array', size=4 * MIB, count=8, codec='array'),
        dict(name='fastdb_mixed', size=64 * 1024, count=40, codec='mixed'),
    ])
    return [{**dict(workers=1, buddy=True, no_shm=False, codec='pickle'), **row}
            for row in rows]


def sized_bytes(encoded_size: int, tag: str) -> bytes:
    length = encoded_size
    while True:
        value = bench.payload_pattern(tag, length)
        difference = len(pickle.dumps(value, protocol=4)) - encoded_size
        if difference == 0:
            return value
        length -= difference


def expected_capacity(row: dict, mode: str) -> bool:
    return bool((row.get('zero_backing') and mode in ('U', 'Q'))
                or (mode == 'S' and row.get('budget_offset', 0) < 0))


def auxiliary_rows():
    common = dict(workers=1, buddy=True, codec='pickle', size=4 * MIB, count=32,
                  kind='mixed', backing_budget=256 * MIB)
    return [dict(common, name='mixed_shm', no_shm=False),
            dict(common, name='mixed_no_shm', no_shm=True),
            dict(name='hold_resume', workers=1, buddy=True, no_shm=False, codec='pickle',
                 size=64 * 1024, count=4, kind='hold')]


def quantiles(values: list[int]) -> dict:
    values = sorted(values)
    return {f'p{percent}_ns': values[math.ceil(percent / 100 * len(values)) - 1]
            if values else None for percent in (50, 95, 99)}


def check(value: bool, message: str, category: str = CORRECTNESS_FAILURE) -> None:
    if not value:
        raise BenchmarkError(message, category)


def summarize_run(row: dict, run: dict) -> dict:
    mode = run['mode']
    check(run['capacity_error'] == expected_capacity(row, mode), 'capacity outcome differs from plan')
    check(run['integrity_ok'], 'content or order differs')
    for sample in run['samples']:
        check(sample['end_ns'] >= sample['start_ns']
              and sample['latency_ns'] == sample['end_ns'] - sample['start_ns'],
              'raw sample clock/latency mismatch', TOOL_FAILURE)
    check(run['end_ns'] >= run['start_ns'], 'terminal precedes call', TOOL_FAILURE)
    if mode == 'S':
        check(run['terminal']['completed'], 'stream cleanup did not complete')
        check(run['terminal']['complete_items'] == run['items'], 'terminal item count differs')
    if not run['capacity_error']:
        check(run['items'] == row['count'], 'delivered item count differs')
        check(len(run['samples']) == (1 if mode == 'U' else row['count'])
              and all(sample['status'] == 'success' for sample in run['samples']),
              'successful run has missing or unsuccessful attempts')
        if mode == 'S':
            check(run['terminal'] == dict(completed=True, error=None, complete_items=row['count']),
                  'stream terminal differs')
    duration = run['end_ns'] - run['start_ns']
    return {
        'first_item_ns': None if run['first_ns'] is None else run['first_ns'] - run['start_ns'],
        'item_wait': quantiles([s['latency_ns'] for s in run['samples']]) if mode != 'U' else None,
        'whole_unary': quantiles([s['latency_ns'] for s in run['samples']]) if mode == 'U' else None,
        'terminal_latency_ns': duration,
        'terminal_after_last_ns': None if run['last_ns'] is None else run['end_ns'] - run['last_ns'],
        'throughput_items_per_s': run['items'] * 1e9 / duration if duration else None,
        'throughput_logical_bytes_per_s': run['logical_bytes'] * 1e9 / duration if duration else None,
    }


def dataset(row: dict):
    """Prepare deterministic domain data before starting the measurement clock."""
    count = row['count']
    items = [sized_bytes(row['encoded_target'], f'{row["name"]}/{i}')
             if 'encoded_target' in row else bench.payload_pattern(f'{row["name"]}/{i}', row['size'])
             for i in range(count)]
    if row['codec'] == 'pickle':
        whole = b''.join(items)
        encoded = [len(pickle.dumps(item, protocol=4)) for item in items]
        return dict(items=items, whole=whole, digests=[hashlib.sha256(v).hexdigest() for v in items],
                    whole_digest=hashlib.sha256(whole).hexdigest(), spec=None,
                    logical_sizes=list(map(len, items)), encoded_sizes=encoded,
                    whole_encoded_size=len(pickle.dumps(whole, protocol=4)))
    return fastdb_dataset(row, items)


def fastdb_dataset(row, raw):
    import struct
    from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, FixedRun
    array_type = {'kind': 'list', 'items': {'kind': 'f64'}}
    fields = [('array', array_type)] if row['codec'] == 'array' else [
        ('index', {'kind': 'i32'}), ('label', {'kind': 'str'}),
        ('blob', {'kind': 'bytes'}), ('array', array_type)]
    spec = dict(schema='fastdb.payload.v1', profile='record.v1', components=[],
                entries=[dict(id=name, cardinality='many', type=kind) for name, kind in fields])
    compiled = CompiledSpec.compile(json.dumps(spec).encode())
    values = []
    for index, blob in enumerate(raw):
        array_size = row['size'] if row['codec'] == 'array' else row['size'] // 2 // 8 * 8
        array = struct.pack('<d', index + 0.5) * (array_size // 8)
        label = f'item-{index}'
        values.append((array, label, blob[:row['size'] - array_size - len(label.encode()) - 4]))

    def build(indices):
        builder = Builder.create(compiled)
        try:
            for field, (name, _) in enumerate(fields):
                builder.entry_begin(field, len(indices))
                for index in indices:
                    array, label, blob = values[index]
                    if name == 'index':
                        builder.value_i32(index)
                    elif name == 'label':
                        builder.value_str(label)
                    elif name == 'blob':
                        builder.value_bytes(blob)
                    else:
                        builder.value_list_begin(len(array) // 8)
                        builder.value_fixed_run(FixedRun(array, len(array) // 8, 8))
            plan = builder.freeze()
        finally:
            builder.close()
        try:
            return plan.execute(BuildPolicy.ALLOW_STAGING).payload
        finally:
            plan.close()
    try:
        items = [build([i]) for i in range(row['count'])]
        whole = build(list(range(row['count'])))
        return dict(items=items, whole=whole, digests=[v.sha256().hex() for v in items],
                    whole_digest=whole.sha256().hex(), spec=spec,
                    logical_sizes=[row['size']] * row['count'],
                    encoded_sizes=[len(v.binary_bytes()) for v in items],
                    whole_encoded_size=len(whole.binary_bytes()))
    finally:
        compiled.close()


def contract(cc, spec):
    class Comparison:
        @cc.read
        def whole(self) -> bytes: ...

        @cc.read
        def item(self, index: int) -> bytes: ...

        @cc.read
        @cc.stream
        def steps(self) -> Iterator[bytes]: ...

        @cc.read
        def ping(self) -> bytes: ...

        @cc.read
        def progress(self) -> int: ...
    if spec is not None:
        from fastdb4py.payload import Payload
        for name in ('whole', 'item', 'steps'):
            method = getattr(Comparison, name)
            method.__annotations__['return'] = Iterator[Payload] if name == 'steps' else Payload
            cc.transfer(output=spec)(method)
    namespace = 'bytes' if spec is None else spec['entries'][0]['id']
    return cc.crm(namespace='c_two.benchmark.stream.' + namespace, version='1.0.0')(Comparison)


class Content:
    def __init__(self, data):
        self.data = data
        self.produced = 0

    def whole(self):
        return self.data['whole']

    def item(self, index):
        return self.data['items'][index]

    def steps(self):
        for item in self.data['items']:
            self.produced += 1
            yield item

    def ping(self):
        return b'x'

    def progress(self):
        return self.produced


def configure(runtime, row, ipc_root, data):
    if os.name != 'nt':
        runtime.set_local_endpoint(root=str(ipc_root))
    overrides = {'chunk_size': FRAME_BYTES}
    if row['no_shm']:
        backing = row.get('backing_budget', 64 * MIB)
        overrides.update(shm_backing_budget_bytes=0, file_backing_budget_bytes=backing,
                         live_reassembly_budget_bytes=backing)
    if row.get('zero_backing'):
        overrides.update(file_backing_budget_bytes=0, live_reassembly_budget_bytes=0)
    bench.configure(runtime, SimpleNamespace(pool_enabled=row['buddy'], segment_size=2 * MIB,
                                             shm_threshold=THRESHOLD, ipc_overrides=overrides))
    budget = 256 * MIB
    if 'budget_offset' in row:
        budget = data['encoded_sizes'][0] + row['budget_offset']
    if row.get('kind') == 'hold':
        budget = 2 * data['encoded_sizes'][0]
    runtime.set_call_execution_limits(payload_buffer_budget_bytes=budget)


def verify_value(value, digest: str, portable: bool) -> None:
    try:
        actual = value.sha256().hex() if portable else hashlib.sha256(value).hexdigest()
        check(actual == digest, 'payload content or order differs')
    finally:
        if portable:
            value.close()


def execute(row, data, proxy, mode, log, request_id, *, measurement=True):
    """Timestamp after SDK decoding; verification and sample IO follow that timestamp."""
    stream = None
    portable = row['codec'] != 'pickle'
    run = dict(mode=mode, start_ns=time.perf_counter_ns(), first_ns=None, last_ns=None,
               items=0, logical_bytes=0, terminal=None, capacity_error=False,
               integrity_ok=True, samples=[])
    start = run['start_ns']

    def received(value, index, begun, whole=False):
        ended = time.perf_counter_ns()
        verify_value(value, data['whole_digest'] if whole else data['digests'][index], portable)
        run['samples'].append(log.sample(
            f'{request_id}/{index}', begun, ended, 'success', measurement=measurement,
            integrity_ok=True))
        if row['count']:
            run['first_ns'] = run['first_ns'] or ended
            run['last_ns'] = ended
        run['items'] += row['count'] if whole else 1
        run['logical_bytes'] += sum(data['logical_sizes']) if whole else data['logical_sizes'][index]
        run['response_end_ns'] = ended

    try:
        if mode == 'U':
            received(proxy.whole(), 0, start, whole=True)
        elif mode == 'Q':
            for index in range(row['count']):
                start = time.perf_counter_ns()
                received(proxy.item(index), index, start)
        else:
            stream = proxy.steps()
            for index in range(row['count']):
                if index:
                    start = time.perf_counter_ns()
                received(next(stream), index, start)
            try:
                next(stream)
            except StopIteration:
                pass
            else:
                raise BenchmarkError('stream delivered an extra item', CORRECTNESS_FAILURE)
            outcome = stream.close(timeout=30)
            run['terminal'] = {**outcome, 'error': str(outcome['error']) if outcome['error'] else None}
    except Exception as exc:
        from c_two import error
        run['response_end_ns'] = time.perf_counter_ns()
        run['samples'].append(log.sample(f'{request_id}/error', start, run['response_end_ns'],
                                          'error', type(exc).__name__, str(exc),
                                          measurement=measurement, integrity_ok=False))
        run['observed_error'] = {'type': type(exc).__name__, 'message': str(exc),
                                 'phase': getattr(exc, 'transport_phase', None)}
        run['capacity_error'] = isinstance(exc, error.CallCapacityExceeded) or 'memory budget cell' in str(exc)
        if not run['capacity_error']:
            raise
    finally:
        if stream is not None and run['terminal'] is None:
            outcome = stream.close(timeout=30)
            run['terminal'] = {**outcome, 'error': str(outcome['error']) if outcome['error'] else None}
        run['end_ns'] = time.perf_counter_ns() if mode == 'S' else run.get('response_end_ns', run['start_ns'])
    log.event('call_result', run=run, measurement=measurement)
    run['summary'] = summarize_run(row, run)
    return run


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2) + '\n', encoding='utf-8')


def wait_marker(path, timeout, children=()):
    deadline = time.perf_counter() + timeout
    while (value := read_marker(path)) is None:
        check(not any(child.poll() is not None for child in children),
              f'child exited before {path.name}', TOOL_FAILURE)
        if time.perf_counter() >= deadline:
            raise BenchmarkError(f'barrier deadline: {path}', TOOL_FAILURE)
        time.sleep(0.005)
    return value


def provenance():
    from c_two import _native
    return {**bench.runtime_provenance(_native), 'source_commit': bench.source_commit(),
            'harness_sha256': bench.native_digest(__file__)}


def assert_refunded(stats):
    scopes = [stats['runtime_outgoing'], stats['server'], *stats['retired']]
    for scope in scopes:
        if scope is not None:
            for name, cell in scope['cells'].items():
                check(cell['used_bytes'] == 0, f'cleanup retains {scope["role"]}/{name}: {cell}')
    for cell in [stats['payload_buffer'], *stats['retired_payload_buffers']]:
        if cell is not None:
            check(cell['used_bytes'] == 0, f'cleanup retains payload buffer: {cell}')
    check(stats['holds']['active_holds'] == stats['holds']['total_held_bytes'] == 0,
          'cleanup retains holds')


def memory_snapshot(runtime):
    snapshot = dict(runtime.memory_stats())
    scopes = [snapshot['runtime_outgoing'], snapshot['server'], *snapshot['retired']]
    cells = [snapshot['payload_buffer'], *snapshot['retired_payload_buffers']]
    for scope in scopes:
        if scope is not None:
            cells.extend(scope['cells'].values())
    for cell in cells:
        if cell is not None:
            check(0 <= cell['used_bytes'] <= cell['peak_bytes'] <= cell['limit_bytes'],
                  f'budget exceeds configured limit: {cell}')
    return snapshot


def close_data(data):
    if data['spec'] is not None:
        for item in [*data['items'], data['whole']]:
            item.close()


def overlapping(samples, large_runs):
    intervals = []
    for run in large_runs:
        if run['mode'] == 'S':
            intervals.append((run['start_ns'], run['end_ns']))
        else:
            intervals.extend((sample['start_ns'], sample['end_ns']) for sample in run['samples'])
    return [sample for sample in samples
            if any(begin <= sample['start_ns'] and sample['end_ns'] <= end for begin, end in intervals)]


def verify_transport(row, mode, server, client, item_size, whole_size):
    """Establish the configured transport with existing budget peaks, not inference from RSS."""
    shm_peak = server['server']['cells']['shm']['peak_bytes']
    outgoing = client['runtime_outgoing']['cells']
    size = whole_size if mode == 'U' else item_size
    capacity = expected_capacity(row, mode)
    if row['no_shm']:
        check(shm_peak == 0, 'SHM-unavailable scenario used SHM', SCENARIO_NOT_ESTABLISHED)
        if not capacity and mode in ('U', 'Q') and size > FRAME_BYTES:
            check(outgoing['file']['peak_bytes'] > 0
                  and outgoing['reassembly']['peak_bytes'] >= size,
                  'file reassembly scenario not established', SCENARIO_NOT_ESTABLISHED)
    elif not capacity:
        check((shm_peak > 0) == (size > THRESHOLD), 'SHM threshold scenario not established', SCENARIO_NOT_ESTABLISHED)
    if mode == 'S' and not capacity:
        check(outgoing['file']['peak_bytes'] == outgoing['reassembly']['peak_bytes'] == 0,
              'stream unexpectedly used unary reassembly')
        check(client['payload_buffer']['peak_bytes'] >= item_size, 'stream B reservation not observable')


def mixed_group(row, data, proxy, mode, log, job_id, output, timeout):
    stop = threading.Event()
    started = threading.Event()
    changed = threading.Condition()
    large_runs, errors, short = [], [], []
    identity = dict(role='large', pid=os.getpid(), worker_index=0)
    large_log = SampleLog(output / job_id.replace('/', '-'), identity, row, provenance())

    def produce():
        try:
            started.set()
            while not stop.is_set():
                run = execute(row, data, proxy, mode, large_log, f'{job_id}/large/{len(large_runs)}')
                with changed:
                    large_runs.append(run)
                    changed.notify_all()
        except Exception as exc:
            with changed:
                errors.append(exc)
                changed.notify_all()

    thread = None if mode == 'alone' else threading.Thread(target=produce, daemon=True)
    deadline = time.perf_counter() + timeout
    begun = time.perf_counter_ns()
    try:
        if thread is not None:
            thread.start()
            check(started.wait(timeout), 'large producer did not start', SCENARIO_NOT_ESTABLISHED)
        selected = []
        while len(selected) < 500:
            for _ in range(500 - len(selected)):
                start = time.perf_counter_ns()
                try:
                    value = proxy.ping()
                    end = time.perf_counter_ns()
                    check(value == b'x', 'short unary content differs')
                    sample = log.sample(f'{job_id}/short/{len(short)}', start, end, 'success', integrity_ok=True)
                except Exception as exc:
                    log.sample(f'{job_id}/short/{len(short)}', start, time.perf_counter_ns(), 'error',
                               type(exc).__name__, str(exc), integrity_ok=False)
                    raise
                short.append(sample)
            if thread is None:
                selected = short
            else:
                with changed:
                    while not errors and (not large_runs or large_runs[-1]['end_ns'] < short[-1]['end_ns']):
                        remaining = deadline - time.perf_counter()
                        check(remaining > 0, 'large/short overlap not established', SCENARIO_NOT_ESTABLISHED)
                        changed.wait(min(remaining, 1))
                    if errors:
                        raise errors[0]
                    selected = overlapping(short, large_runs)
            check(time.perf_counter() < deadline, '500 overlapping calls not established', SCENARIO_NOT_ESTABLISHED)
    finally:
        stop.set()
        if thread is not None:
            thread.join(timeout)
            check(not thread.is_alive(), 'large call did not finish')
        large_log.close()
    if errors:
        raise errors[0]
    selected = selected[:500]
    elapsed = time.perf_counter_ns() - begun
    return dict(mode=mode, job_id=job_id, short_samples=short, large_runs=large_runs,
                selected_short_ids=[sample['request_id'] for sample in selected],
                short_latency=quantiles([sample['latency_ns'] for sample in selected]),
                short_attempts=len(short), overlapping_samples=len(selected), short_failure_rate=0.0,
                large_throughput_logical_bytes_per_s=sum(run['logical_bytes'] for run in large_runs) * 1e9 / elapsed,
                elapsed_ns=elapsed,
                large_samples_path=str(large_log.samples_path.relative_to(output)),
                large_events_path=str(large_log.events_path.relative_to(output)))


def hold_diagnostic(row, data, proxy, runtime, log, timeout):
    import c_two as cc
    stream = cc.hold(proxy.steps)()
    held, delivered = [], {}
    finished = threading.Event()
    thread = None
    try:
        held.extend([next(stream), next(stream)])
        full = memory_snapshot(runtime)
        check(full['payload_buffer']['used_bytes'] == full['payload_buffer']['limit_bytes']
              == 2 * data['encoded_sizes'][0], 'two holds did not fill B')
        check(full['holds']['active_holds'] == 2, 'two holds not observable')

        def take():
            try:
                delivered['item'] = next(stream)
            except Exception as exc:
                delivered['error'] = exc
            finally:
                finished.set()
        thread = threading.Thread(target=take, daemon=True)
        thread.start()
        deadline = time.perf_counter() + timeout
        while proxy.progress() < 3:
            check(time.perf_counter() < deadline, 'third resource step not established', SCENARIO_NOT_ESTABLISHED)
            time.sleep(0.005)
        check(not finished.is_set(), 'third item crossed a full B')
        log.event('hold_budget_full', memory=full, produced=proxy.progress())
        first = held.pop(0)
        check(hashlib.sha256(first.value).hexdigest() == data['digests'][0], 'first held item differs')
        first.release()
        thread.join(timeout)
        check(finished.is_set() and 'error' not in delivered, f'hold release did not resume next: {delivered.get("error")}')
        held.append(delivered['item'])
        for index in (1, 2):
            item = held.pop(0)
            check(hashlib.sha256(item.value).hexdigest() == data['digests'][index], 'held content differs')
            item.release()
        fourth = next(stream)
        try:
            check(hashlib.sha256(fourth.value).hexdigest() == data['digests'][3], 'fourth held item differs')
        finally:
            fourth.release()
        try:
            next(stream)
        except StopIteration:
            pass
        else:
            raise BenchmarkError('hold stream has an extra item', CORRECTNESS_FAILURE)
        terminal = stream.close(timeout=timeout)
        check(terminal == dict(completed=True, error=None, complete_items=4), 'hold terminal differs')
        after = memory_snapshot(runtime)
        check(after['payload_buffer']['used_bytes'] == 0
              and after['payload_buffer']['rejected_allocations'] == 0
              and after['holds']['active_holds'] == 0, 'hold resume rejected or retained data')
        return dict(mode='S', full=full, after=after, terminal=terminal)
    finally:
        for item in held:
            item.release()
        stream.close(timeout=timeout)
        if thread is not None:
            thread.join(timeout)


def worker(spec, index):
    import c_two as cc
    from c_two.observe import connection_observation
    row = spec['row']
    output = Path(spec['output'])
    coord = output / 'coord'
    data = dataset(row)
    runtime = cc.Runtime()
    configure(runtime, row, spec['ipc_root'], data)
    proxy = None
    report = dict(index=index, provenance=provenance(), runs=[], failures=[])
    identity = dict(role='client', pid=os.getpid(), worker_index=index)
    with SampleLog(output, identity, row, report['provenance']) as log:
        report.update(samples_path=log.samples_path.name, events_path=log.events_path.name)
        try:
            proxy = runtime.connect(contract(cc, data['spec']), name='content', address=spec['address'])
            check(proxy.client.observed_path == 'direct_ipc', 'direct IPC not established', SCENARIO_NOT_ESTABLISHED)
            report['connection_id'] = connection_observation(proxy).connection_id
            if spec['phase'] == 'performance':
                for mode in spec['modes']:
                    if mode in MODES:
                        execute(row, data, proxy, mode, log, 'warmup/' + mode, measurement=False)
            for job in spec['jobs']:
                barrier = coord / job['id']
                bench.wait_for_release(barrier, index, spec['timeout'], log)
                before = process_snapshot() if spec['phase'] == 'diagnostic' else None
                cpu_start = time.process_time_ns()
                if row.get('kind') == 'mixed':
                    run = mixed_group(row, data, proxy, job['mode'], log, job['id'], output, spec['timeout'])
                elif row.get('kind') == 'hold':
                    run = hold_diagnostic(row, data, proxy, runtime, log, spec['timeout'])
                else:
                    run = execute(row, data, proxy, job['mode'], log, job['id'])
                cpu_end = time.process_time_ns()
                run.update(job_id=job['id'], worker_index=index)
                if spec['phase'] == 'diagnostic':
                    run['diagnostics'] = dict(cpu_ns=cpu_end - cpu_start, rss_before=before,
                                              rss_after=process_snapshot(), memory=memory_snapshot(runtime))
                report['runs'].append(run)
                publish_marker(barrier / f'done-{index}.json', {'pid': os.getpid()})
        except Exception as exc:
            report['failures'].append(dict(category=getattr(exc, 'category', TOOL_FAILURE),
                                           message=f'{type(exc).__name__}: {exc}'))
        finally:
            try:
                if proxy is not None:
                    cc.close(proxy)
                report['shutdown'] = dict(runtime.shutdown(timeout=30))
                check(report['shutdown']['completed'], 'client cleanup did not complete')
                report['after_cleanup'] = memory_snapshot(runtime)
                assert_refunded(report['after_cleanup'])
            except Exception as exc:
                report['failures'].append(dict(category=getattr(exc, 'category', TOOL_FAILURE),
                                               message=f'cleanup: {exc}'))
            close_data(data)
            write_json(output / f'worker-{index}.json', report)
    return 1 if report['failures'] else 0


def controller(spec):
    import c_two as cc
    row = spec['row']
    output = Path(spec['output'])
    data = dataset(row)
    runtime = cc.Runtime()
    configure(runtime, row, spec['ipc_root'], data)
    children, handles = [], []
    failures = Failures()
    report = dict(row=row, phase=spec['phase'], provenance=provenance(), workers=[],
                  encoded_sizes=data['encoded_sizes'], whole_encoded_size=data['whole_encoded_size'],
                  logical_sizes=data['logical_sizes'], server_measurements=[])
    with SampleLog(output, dict(role='server', pid=os.getpid(), worker_index=-1), row,
                   report['provenance']) as log:
        try:
            runtime.register(contract(cc, data['spec']), Content(data), name='content')
            spec['address'] = runtime.server_address()
            spec_file = output / 'worker-spec.json'
            write_json(spec_file, spec)
            for job in spec['jobs']:
                (output / 'coord' / job['id']).mkdir(parents=True)
            for index in range(row['workers']):
                handle = (output / f'worker-{index}.log').open('w')
                handles.append(handle)
                children.append(subprocess.Popen(
                    [spec['python'], __file__, '--worker', str(spec_file), '--index', str(index)],
                    stdout=handle, stderr=subprocess.STDOUT, cwd=ROOT))
            for job in spec['jobs']:
                barrier = output / 'coord' / job['id']
                # Wait for every warmed client before starting server-side counters.
                for index in range(len(children)):
                    wait_marker(barrier / f'ready-{index}.json', spec['timeout'], children)
                rss = process_snapshot() if spec['phase'] == 'diagnostic' else None
                cpu_start = time.process_time_ns()
                released = bench.release_workers(barrier, children, spec['timeout'], log)
                for index in range(len(children)):
                    wait_marker(barrier / f'done-{index}.json', spec['timeout'])
                ended = time.perf_counter_ns()
                if spec['phase'] == 'diagnostic':
                    report['server_measurements'].append(dict(
                        job_id=job['id'], cpu_ns=time.process_time_ns() - cpu_start,
                        wall_ns=ended - released['release_ns'], rss_before=rss,
                        rss_after=process_snapshot(), memory=memory_snapshot(runtime)))
            for child in children:
                child.wait(timeout=spec['timeout'])
            for index, child in enumerate(children):
                record = json.loads((output / f'worker-{index}.json').read_text())
                report['workers'].append(record)
                for failure in record['failures']:
                    failures.append(f'worker {index}: {failure["message"]}', failure['category'])
                check(child.returncode == (1 if record['failures'] else 0),
                      'worker exit code disagrees with its report', TOOL_FAILURE)
        except Exception as exc:
            failures.append(f'{type(exc).__name__}: {exc}', getattr(exc, 'category', TOOL_FAILURE))
        finally:
            for child in children:
                if child.poll() is None:
                    detail = bench.kill_owned_child(child)
                    failures.tool('worker required forced termination' + (f': {detail}' if detail else ''))
            for handle in handles:
                handle.close()
            # A worker can fail before readiness; retain its original cause.
            recorded = {record['index'] for record in report['workers']}
            for index in range(len(children)):
                path = output / f'worker-{index}.json'
                if index not in recorded and path.is_file():
                    record = json.loads(path.read_text())
                    report['workers'].append(record)
                    for failure in record['failures']:
                        failures.append(f'worker {index}: {failure["message"]}', failure['category'])
            try:
                report['shutdown'] = dict(runtime.shutdown(timeout=30))
                check(report['shutdown']['completed'], 'server cleanup did not complete')
                report['after_cleanup'] = memory_snapshot(runtime)
                assert_refunded(report['after_cleanup'])
            except Exception as exc:
                failures.append(f'server cleanup: {exc}', getattr(exc, 'category', TOOL_FAILURE))
            close_data(data)
            report.update(ok=not failures, failures=list(failures))
            write_json(output / 'row.json', report)
    return 0 if report['ok'] else 1


def run_phase(args, row, phase, output, modes=MODES):
    output.mkdir(parents=True)
    groups = args.groups if phase == 'performance' else 1
    jobs = [dict(id=f'{group}/{mode}', mode=mode) for group in range(groups)
            for mode in (modes[group % len(modes):] + modes[:group % len(modes)])]
    # Runtime shutdown retires its endpoints. Preserve the task-owned root and
    # coordination files: the caller forbids removing directories outside ROOT.
    ipc_root = tempfile.mkdtemp(prefix='c2s-', dir=os.environ.get('TMPDIR'))
    spec = dict(row=row, output=str(output.resolve()), phase=phase, python=str(args.python),
                ipc_root=ipc_root, modes=list(modes), jobs=jobs, timeout=args.child_timeout)
    spec_path = output / 'invocation.json'
    write_json(spec_path, spec)
    environment = dict(os.environ, C2_ENV_FILE='', C2_RELAY_ANCHOR_ADDRESS='',
                       C2_BENCH_SOURCE_ROOT=str(ROOT), C2_BENCH_SIDE='candidate')
    with (output / 'process.log').open('w') as log:
        result = run_owned([str(args.python), __file__, '--controller', str(spec_path)],
                           cwd=ROOT, env=environment, stdout=log, timeout=args.row_timeout)
    path = output / 'row.json'
    check(path.is_file(), f'missing row report: {path}; see process.log', TOOL_FAILURE)
    report = json.loads(path.read_text())
    check(result.returncode == (0 if report['ok'] else 1), 'controller exit/report mismatch', TOOL_FAILURE)
    return report


def verify_phase(report, directory, row, groups, modes, expected_commit):
    """Recompute from raw attempts, and bind every process to the imported binary."""
    check(report['row'] == row, 'reported row/configuration differs', TOOL_FAILURE)
    check(len(report['workers']) == row['workers'], 'worker count differs', TOOL_FAILURE)
    check({worker['index'] for worker in report['workers']} == set(range(row['workers'])),
          'missing/duplicate worker identity', TOOL_FAILURE)
    native = report['provenance']
    check(native['source_commit'] == expected_commit and native['sha256']
          and len(native['sha256']) == 64 and native['module_path'], 'missing native identity', TOOL_FAILURE)
    check(Path(native['module_path']).is_relative_to(ROOT), 'native imported outside candidate checkout', TOOL_FAILURE)
    check(bench.native_digest(native['module_path']) == native['sha256'], 'imported native hash differs', TOOL_FAILURE)
    check(bench.native_digest(__file__) == native['harness_sha256'], 'harness changed during measurement', TOOL_FAILURE)
    expected_jobs = {f'{group}/{mode}' for group in range(groups) for mode in modes}
    if report['phase'] == 'diagnostic' and not row.get('kind'):
        mode = modes[0]
        for worker_report in report['workers']:
            verify_transport(row, mode, report['server_measurements'][0]['memory'],
                             worker_report['runs'][0]['diagnostics']['memory'],
                             max(report['encoded_sizes'], default=0), report['whole_encoded_size'])
    combined = {mode: [] for mode in modes}
    summaries = {}
    for worker_report in report['workers']:
        identity = worker_report['provenance']
        check(all(identity[key] == native[key] for key in ('source_commit', 'sha256', 'harness_sha256')),
              'server/client native or source identity differs', TOOL_FAILURE)
        check({run['job_id'] for run in worker_report['runs']} == expected_jobs
              and len(worker_report['runs']) == len(expected_jobs), 'missing/duplicate measured run', TOOL_FAILURE)
        records = [json.loads(line) for line in (directory / worker_report['samples_path']).read_text().splitlines()]
        check(records[0]['kind'] == 'run' and records[0]['config'] == row
              and records[0]['provenance'] == identity, 'sample header differs', TOOL_FAILURE)
        raw = [record for record in records if record.get('kind') == 'sample' and record['measurement']]
        runs = worker_report['runs']
        kind = row.get('kind')
        key = 'short_samples' if kind == 'mixed' else 'samples'
        check(raw == [sample for run in runs for sample in run.get(key, [])], 'raw attempts differ from receipt', TOOL_FAILURE)
        if kind == 'hold':
            run = runs[0]
            check(len(runs) == 1 and run['terminal'] == dict(completed=True, error=None, complete_items=4),
                  'hold terminal differs')
            full, after = run['full'], run['after']
            check(full['holds']['active_holds'] == 2
                  and full['payload_buffer']['used_bytes'] == full['payload_buffer']['limit_bytes']
                  == 2 * report['encoded_sizes'][0], 'hold saturation not established')
            check(after['payload_buffer']['used_bytes'] == after['holds']['active_holds']
                  == after['payload_buffer']['rejected_allocations'] == 0, 'hold release did not settle')
            return dict(full=full, after=after, terminal=run['terminal'])
        if kind == 'mixed':
            for mode in modes:
                selected_all = []
                for run in (run for run in runs if run['mode'] == mode):
                    short, large = run['short_samples'], run['large_runs']
                    for sample in short:
                        check(sample['status'] == 'success' and sample['integrity_ok'], 'short unary failed')
                        check(sample['latency_ns'] == sample['end_ns'] - sample['start_ns'],
                              'short sample timing differs', TOOL_FAILURE)
                    selected = (short if mode == 'alone' else overlapping(short, large))[:500]
                    check(len(selected) == 500, '500 overlapping samples missing', SCENARIO_NOT_ESTABLISHED)
                    check(run['selected_short_ids'] == [s['request_id'] for s in selected]
                          and run['short_latency'] == quantiles([s['latency_ns'] for s in selected]),
                          'mixed summary differs from raw overlap', TOOL_FAILURE)
                    large_raw = [json.loads(line) for line in (directory / run['large_samples_path']).read_text().splitlines()]
                    check(large_raw[0]['provenance']['sha256'] == native['sha256'], 'mixed native differs', TOOL_FAILURE)
                    check([s for s in large_raw if s.get('kind') == 'sample' and s['measurement']]
                          == [s for big in large for s in big['samples']], 'large samples differ', TOOL_FAILURE)
                    for big in large:
                        check(big['summary'] == summarize_run(row, big), 'large summary differs', TOOL_FAILURE)
                    selected_all.extend(selected)
                summaries[mode] = dict(short_latency=quantiles([s['latency_ns'] for s in selected_all]),
                                       overlapping_samples=len(selected_all), short_failure_rate=0.0)
            if 'alone' in modes and set(MODES) <= set(modes):
                for mode in MODES:
                    summaries[mode]['short_p99_ratio_to_alone'] = (
                        summaries[mode]['short_latency']['p99_ns'] / summaries['alone']['short_latency']['p99_ns'])
            return summaries
        for run in runs:
            check(run['summary'] == summarize_run(row, run), 'summary differs from raw timing', TOOL_FAILURE)
            check(run['job_id'].endswith('/' + run['mode']), 'mode differs from job', TOOL_FAILURE)
        for mode in modes:
            selected = [run for run in runs if run['mode'] == mode]
            combined[mode].extend(selected)
            samples = [sample for run in selected for sample in run['samples']]
            duration = sum(run['end_ns'] - run['start_ns'] for run in selected)
            summaries[f'{mode}/worker-{worker_report["index"]}'] = dict(
                first_item=quantiles([run['summary']['first_item_ns'] for run in selected
                                      if run['first_ns'] is not None]),
                item_wait=quantiles([sample['latency_ns'] for sample in samples]) if mode != 'U' else None,
                whole_unary=quantiles([sample['latency_ns'] for sample in samples]) if mode == 'U' else None,
                terminal=quantiles([run['summary']['terminal_latency_ns'] for run in selected]),
                terminal_after_last=quantiles([run['summary']['terminal_after_last_ns'] for run in selected
                                               if run['last_ns'] is not None]),
                attempts=len(samples), errors=sum(sample['status'] != 'success' for sample in samples),
                expected_capacity_failures=sum(run['capacity_error'] for run in selected),
                throughput_items_per_s=sum(run['items'] for run in selected) * 1e9 / duration if duration else None,
                throughput_logical_bytes_per_s=sum(run['logical_bytes'] for run in selected) * 1e9 / duration if duration else None)
    for mode, runs in combined.items():
        envelopes = []
        for group in range(groups):
            selected = [run for run in runs if run['job_id'] == f'{group}/{mode}']
            envelopes.append(max(run['end_ns'] for run in selected) - min(run['start_ns'] for run in selected))
        duration = sum(envelopes)
        summaries[mode + '/aggregate'] = dict(
            throughput_items_per_s=sum(run['items'] for run in runs) * 1e9 / duration if duration else None,
            throughput_logical_bytes_per_s=sum(run['logical_bytes'] for run in runs) * 1e9 / duration if duration else None)
    return summaries


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output-dir', type=Path)
    parser.add_argument('--python', type=Path, default=Path(sys.executable))
    parser.add_argument('--only', help='Comma-separated row names; default is the full matrix')
    parser.add_argument('--groups', type=int, default=5)
    parser.add_argument('--child-timeout', type=float, default=120)
    parser.add_argument('--row-timeout', type=float, default=600)
    parser.add_argument('--worker', type=Path)
    parser.add_argument('--controller', type=Path)
    parser.add_argument('--index', type=int)
    args = parser.parse_args()
    if args.worker:
        raise SystemExit(worker(json.loads(args.worker.read_text()), args.index))
    if args.controller:
        raise SystemExit(controller(json.loads(args.controller.read_text())))
    if args.output_dir is None or args.groups < 5:
        parser.error('--output-dir and at least five groups are required')
    for timeout in (args.child_timeout, args.row_timeout):
        if not math.isfinite(timeout) or timeout <= 0:
            parser.error('timeouts must be finite and positive')
    rows = matrix_rows() + auxiliary_rows()
    selected = set(args.only.split(',')) if args.only else {row['name'] for row in rows}
    if not selected <= {row['name'] for row in rows}:
        parser.error('unknown row selector')
    # Resolving a venv symlink would select the base interpreter without its SDK.
    args.python = args.python.absolute()
    args.output_dir.mkdir(parents=True, exist_ok=False)
    receipt = dict(schema=SCHEMA, source_commit=bench.source_commit(),
                   harness_sha256=bench.native_digest(__file__),
                   machine=platform.uname()._asdict(), selected_rows=sorted(selected),
                   groups=args.groups, rows=[], failures=[],
                   comparison_rows=[row['name'] for row in matrix_rows()],
                   auxiliary_rows=[row['name'] for row in auxiliary_rows()],
                   measurement_notes={
                       'latency_clock': {'name': 'perf_counter_ns', **vars(time.get_clock_info('perf_counter'))},
                       'item_wait': 'Q call / S next including SDK decode; first S includes opening',
                       'throughput': 'successful items or logical bytes per call envelope, including consumer verification and evidence IO',
                       'cpu': 'process CPU over the synchronized diagnostic interval, including harness work',
                       'rss': 'OS process RSS includes prepared content, codecs, Python and native memory; not B',
                       'overlap': 'complete short call inside U/Q call intervals or the S call lifetime; same proxy/connection',
                       'peaks': 'diagnostic modes use fresh processes; never sum independent memory scopes'},
                   copy_count=dict(value=None, reason='No existing copy-count observer; no product probe added'))
    started = time.perf_counter_ns()
    for row in rows:
        if row['name'] not in selected:
            continue
        begun = time.perf_counter_ns()
        record = dict(name=row['name'], phases=[])
        try:
            modes = MODES + ('alone',) if row.get('kind') == 'mixed' else MODES
            phases = [('performance', modes), *[('diagnostic', (mode,)) for mode in modes]]
            if row.get('kind') == 'hold':
                phases = [('diagnostic', ('S',))]
            for phase, modes in phases:
                suffix = phase if phase == 'performance' else phase + '-' + modes[0]
                directory = args.output_dir / row['name'] / suffix
                report = run_phase(args, row, phase, directory, modes)
                record['phases'].append(str(directory.relative_to(args.output_dir) / 'row.json'))
                check(report['provenance']['harness_sha256'] == receipt['harness_sha256'],
                      'harness changed between phases', TOOL_FAILURE)
                if not report['ok']:
                    receipt['failures'].extend(report['failures'])
                    record['failure'] = '; '.join(report['failures'])
                    break
                groups = args.groups if phase == 'performance' else 1
                record.setdefault('summaries', {})[suffix] = verify_phase(
                    report, directory, row, groups, modes, receipt['source_commit'])
        except Exception as exc:
            record['failure'] = str(exc)
            record['failure_traceback'] = traceback.format_exc()
            receipt['failures'].append(failure_message(str(exc), getattr(exc, 'category', TOOL_FAILURE), 'candidate'))
        record['elapsed_ns'] = time.perf_counter_ns() - begun
        receipt['rows'].append(record)
        receipt['elapsed_ns'] = time.perf_counter_ns() - started
        receipt['ok'] = not receipt['failures']
        receipt['full_matrix'] = not args.only and len(receipt['rows']) == len(rows)
        write_json(args.output_dir / 'receipt.json', receipt)
        print(f'{row["name"]}: {record["elapsed_ns"] / 1e9:.3f}s ok={"failure" not in record}', flush=True)
        if receipt['failures']:
            break  # An observed outcome different from the plan requires review.
    raise SystemExit(0 if receipt['ok'] else 1)


if __name__ == '__main__':
    main()
