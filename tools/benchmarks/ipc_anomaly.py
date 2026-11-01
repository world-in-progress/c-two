"""Public-API anomaly workloads with owned-process stop/resume coordination.

A stopped reader is gated in the resource before the controller stops its
process. Only then does the resource return the response. Internal loan/queue
measurements remain explicitly unavailable. A blocked large response is an OS
observation, without a claim about its exact partial-write position.
"""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import traceback

import ipc_memory as bench
from ipc_evidence import (
    INTERNAL_PROBES, SampleLog, process_snapshot, resume_process, suspend_process,
    summarize_samples,
)

MIB = 1 << 20
GATED = {'slow', 'stopped', 'fallback_dedicated', 'fallback_inline', 'fallback_file',
         'exhausted', 'release_blocked', 'release_batch'}
STOPPED = GATED - {'slow'}

# Existing source evidence, not test results produced by this tool.
RELEASE_TESTS = [
    {'file': 'core/protocol/c2-wire/src/connection/tests.rs',
     'test': 'remote_read_drop_queues_release_and_wakes_idle_writer', 'behavior': 'idle writer wake'},
    {'file': 'core/protocol/c2-wire/src/connection/tests.rs',
     'test': 'release_queue_batches_and_cosubmits_with_one_business_frame', 'behavior': '130 records in three release frames, co-submitted with business'},
    {'file': 'core/protocol/c2-wire/src/connection/tests.rs',
     'test': 'releases_wait_for_current_frame_and_advance_between_chunks', 'behavior': 'frame exclusion and progress between chunks'},
    {'file': 'core/protocol/c2-wire/src/connection/tests.rs',
     'test': 'partial_write_cancellation_keeps_loans_and_never_replays_release_prefix', 'behavior': 'partial-write cancellation retains published storage, no replay'},
    {'file': 'core/transport/c2-server/src/writer.rs',
     'test': 'queued_release_precedes_business_in_one_core_batch', 'behavior': 'release prefix precedes business in real stream submission'},
    {'file': 'core/transport/c2-server/src/writer.rs',
     'test': 'timeout_keeps_the_same_batch_through_its_release_prefix_and_flush', 'behavior': 'four-byte duplex backpressure, timeout and recovery retain batch'},
    {'file': 'core/transport/c2-ipc/src/p4_tests.rs',
     'test': 'real_response_is_released_before_local_copy_is_held_and_close', 'behavior': 'response SHM released before retaining local copy'},
    {'file': 'core/transport/c2-ipc/src/p4_tests.rs',
     'test': 'cancelling_a_started_shm_write_keeps_the_driver_and_never_replays', 'behavior': 'started write survives caller cancellation'},
    {'file': 'core/transport/c2-server/src/server_tests.rs',
     'test': 'raw_buddy_request_releases_before_blocked_callback_completes', 'behavior': 'request release precedes blocked callback completion'},
    {'file': 'core/transport/c2-server/src/server_tests.rs',
     'test': 'raw_response_release_after_reply_settles_loan', 'behavior': 'received release settles original response allocation'},
]


def publish(path: Path, value: dict | None = None) -> None:
    pending = path.with_suffix('.pending')
    pending.write_text(json.dumps(value or {'monotonic_ns': time.monotonic_ns()}), encoding='utf-8')
    pending.replace(path)


def wait_marker(path: Path, timeout: float, children=(), tick=None) -> dict:
    deadline = time.monotonic() + timeout
    while not path.exists():
        if any(child.poll() is not None for child in children):
            raise RuntimeError(f'child exited before {path.name}')
        if time.monotonic() >= deadline:
            raise TimeoutError(f'coordination deadline: {path.name}')
        if tick is not None:
            tick()
        time.sleep(0.005)
    return json.loads(path.read_text(encoding='utf-8'))


def wait_for_entered(coord: Path, expected: int, timeout: float, children=(), tick=None) -> int:
    """Bound the whole resource-entry barrier and retain its actual progress."""
    paths = [coord / f'entered-abnormal-{index}.json' for index in range(expected)]
    deadline = time.monotonic() + timeout
    while True:
        entered = sum(path.exists() for path in paths)
        if entered == expected:
            return entered
        progress = f'resource entry incomplete: entered={entered}, expected={expected}'
        if any(child.poll() is not None for child in children):
            raise RuntimeError(progress + '; child exited')
        if time.monotonic() >= deadline:
            raise TimeoutError(progress + '; coordination deadline')
        if tick is not None:
            tick()
        time.sleep(0.005)


def scenario_spec(args: argparse.Namespace) -> dict:
    """Fixed geometry is identical on both inputs and in both controls."""
    common = {
        'pool_enabled': True, 'pool_segment_size': MIB, 'max_pool_segments': 1,
        'pool_min_retained_segments': 1, 'pool_decay_seconds': 0,
        'reassembly_segment_size': MIB, 'reassembly_max_segments': 1,
        'shm_backing_budget_bytes': 32 * MIB, 'file_backing_budget_bytes': 32 * MIB,
        'live_reassembly_budget_bytes': 32 * MIB, 'chunk_size': 32 * 1024,
    }
    server = dict(common, max_frame_size=64 * 1024)
    normal, abnormal = dict(common), dict(common)
    scenario = args.scenario
    response = 300 * 1024  # rounded to 512 KiB in the 1 MiB buddy geometry
    request = 1024
    abnormal_response, abnormal_request = response, request
    inflight = 1
    if scenario.startswith('fallback_') or scenario == 'exhausted':
        inflight = 2  # two responses consume both 512 KiB buddy blocks
        if scenario != 'fallback_dedicated':
            server['shm_backing_budget_bytes'] = MIB
        if scenario in ('fallback_file', 'exhausted'):
            normal['shm_backing_budget_bytes'] = 0
        if scenario == 'exhausted':
            for overrides in (server, normal):
                overrides['file_backing_budget_bytes'] = 0
                overrides['live_reassembly_budget_bytes'] = 0
    if scenario == 'release_batch':
        inflight = max(65, args.abnormal_calls)
        abnormal_response = 8192
        # Every gated call needs an execution permit at once; keep a separate
        # permit available for the normal client while those calls are gated.
        server['max_execution_workers'] = inflight + 1
    if scenario == 'release_busy':
        abnormal_request = 256 * 1024
    if scenario == 'release_blocked':
        abnormal_response = 8 * MIB
        server['max_frame_size'] = 16 * MIB
        server['shm_backing_budget_bytes'] = 0  # force one large stream frame
    return {
        'scenario': scenario, 'control': args.control, 'calls': args.calls,
        'warmup': args.warmup, 'abnormal_calls': args.abnormal_calls,
        'pause_seconds': args.pause_seconds, 'call_timeout': args.call_timeout,
        'child_timeout': args.child_timeout, 'inflight_calls': inflight,
        'response_bytes': response, 'request_bytes': request,
        'abnormal_response_bytes': abnormal_response,
        'abnormal_request_bytes': abnormal_request,
        'shm_threshold': 4096, 'server_overrides': server,
        'normal_overrides': normal, 'abnormal_overrides': abnormal,
        'route_concurrency': {'mode': 'parallel', 'max_workers': None, 'max_pending': None},
    }


def workload_types(cc, coord: Path, spec: dict):
    @cc.crm(namespace='c_two.benchmark.anomaly', version='1.0.0')
    class Exchange:
        def exchange(self, value: bytes, response_bytes: int, token: str, gated: bool) -> bytes: ...

    class Resource:
        def exchange(self, value: bytes, response_bytes: int, token: str, gated: bool) -> bytes:
            # Verify incoming contents as well as the returned contents. The
            # token binds each request/response to its unique worker attempt.
            if value != bench.payload_pattern('request-' + token, len(value)):
                raise ValueError('request payload mismatch')
            if gated:
                publish(coord / f'entered-{token}.json')
                wait_marker(coord / 'method-release.json', spec['child_timeout'])
                publish(coord / f'returning-{token}.json')
            return bench.payload_pattern('response-' + token, response_bytes)
    return Exchange, Resource


def _snapshot(log: SampleLog, phase: str) -> dict:
    snapshot = {'monotonic_ns': time.monotonic_ns(), 'os': process_snapshot(),
                'memory': bench.snapshot_memory_stats()}
    if not bench.memory_stats_usable(snapshot['memory']):
        raise RuntimeError(f"memory_stats unavailable at {phase}: {snapshot['memory']}")
    log.event('snapshot', phase=phase, snapshot=snapshot)
    return snapshot


def _invoke(log, proxy, request_id, request_bytes, response_bytes, phase, *, gated=False,
            measurement=True, hold=None):
    token = f"{log.identity['role']}-{request_id}"
    request = bench.payload_pattern('request-' + token, request_bytes)
    expected = bench.payload_pattern('response-' + token, response_bytes)
    def call(_):
        if hold is not None:
            held = hold(proxy.exchange)(request, response_bytes, token, gated)
            return held
        return proxy.exchange(request, response_bytes, token, gated)
    held_box = []
    def checked(value):
        result = call(value)
        if hold is not None:
            held_box.append((result, expected))
            return result.value
        return result
    value, sample = bench.call_sample(log, request_id, checked, expected,
                                      phase=phase, measurement=measurement)
    return sample, held_box


def anomaly_worker(spec: dict) -> dict:
    """One client process. No harness-internal Core entrypoints are used."""
    os.environ['C2_ENV_FILE'] = ''
    os.environ['C2_RELAY_ANCHOR_ADDRESS'] = ''
    import c_two as cc
    from c_two import _native
    role = spec['role']
    coord, output = Path(spec['coord_dir']), Path(spec['output_dir'])
    config = spec['config']
    provenance = dict(bench.runtime_provenance(_native), source_commit=bench.source_commit())
    log = SampleLog(output, {'role': role, 'pid': os.getpid(), 'worker_index': spec['index']}, config, provenance)
    failures, snapshots, cleanup = [], {}, {}
    client = None
    holds = []
    try:
        cc.set_transport_policy(shm_threshold=config['shm_threshold'])
        cc.set_client(ipc_overrides=config[role + '_overrides'])
        Exchange, _ = workload_types(cc, coord, config)
        client = cc.connect(Exchange, name='exchange', address=spec['address'], timeout=config['call_timeout'])
        proxy = cc.with_call_options(client, timeout=config['call_timeout'])
        for index in range(config['warmup']):
            sample, _ = _invoke(log, proxy, f'warmup-{index}', 1024, 8192, 'warmup', measurement=False)
            if sample['status'] != 'success':
                raise RuntimeError(f"warmup: {sample['error_message']}")
        snapshots['ready'] = _snapshot(log, 'ready')
        bench.wait_for_release(coord, spec['index'], config['child_timeout'], log)
        if role == 'normal':
            request_id = 0
            for phase in ('before', 'during', 'after'):
                wait_marker(coord / f'normal-{phase}.json', config['child_timeout'])
                log.event('measurement_started', phase=phase)
                for _ in range(config['calls']):
                    sample, _ = _invoke(log, proxy, request_id, config['request_bytes'], config['response_bytes'], phase)
                    request_id += 1
                    expected_failure = config['scenario'] == 'exhausted' and config['control'] == 'parallel' and phase == 'during'
                    if expected_failure:
                        if sample['status'] != 'error' or bench.BUDGET_ERROR_PHRASE not in (sample['error_message'] or ''):
                            failures.append(f'expected call-stage capacity error: {sample}')
                    elif sample['status'] != 'success':
                        failures.append(f"normal {phase} request {sample['request_id']}: {sample['error_type']}: {sample['error_message']}")
                snapshots[phase] = _snapshot(log, phase)
                publish(coord / f'normal-{phase}-done.json')
            from c_two.transport.client import util
            if not util.ping(spec['address'], timeout=2.0):
                failures.append('control ping failed after workload')
        else:
            wait_marker(coord / 'abnormal-start.json', config['child_timeout'])
            scenario = config['scenario']
            if scenario in GATED:
                # All calls share this one physical connection. The resource
                # gates them before returning, so stop cannot race reception.
                with ThreadPoolExecutor(max_workers=config['inflight_calls']) as executor:
                    futures = [executor.submit(
                        _invoke, log, proxy, index, config['abnormal_request_bytes'],
                        config['abnormal_response_bytes'], 'interference', gated=True,
                    ) for index in range(config['inflight_calls'])]
                    for future in futures:
                        sample, _ = future.result()
                        if sample['status'] != 'success':
                            failures.append(f"abnormal call failed: {sample['error_message']}")
                publish(coord / 'abnormal-finished.json')
            else:
                count = config['abnormal_calls']
                for index in range(count):
                    sample, retained = _invoke(
                        log, proxy, index, config['abnormal_request_bytes'],
                        config['abnormal_response_bytes'], 'interference',
                        hold=cc.hold if scenario == 'hold' else None,
                    )
                    holds.extend(retained)
                    if sample['status'] != 'success':
                        raise RuntimeError(f"abnormal call failed: {sample['error_message']}")
                    if scenario == 'release_busy' and index == 0:
                        snapshots['active'] = _snapshot(log, 'active')
                        publish(coord / 'abnormal-active.json')
                if scenario != 'release_busy':
                    snapshots['active'] = _snapshot(log, 'active')
                    publish(coord / 'abnormal-active.json')
                request_id = count
                while not (coord / 'abnormal-stop.json').exists():
                    if scenario == 'release_busy':
                        sample, _ = _invoke(log, proxy, request_id, config['abnormal_request_bytes'],
                                            config['abnormal_response_bytes'], 'interference')
                        request_id += 1
                        if sample['status'] != 'success':
                            raise RuntimeError(f"busy call failed: {sample['error_message']}")
                    else:
                        time.sleep(0.005)
                for held, expected in holds:
                    if held.value != expected:
                        failures.append('retained local copy changed while normal client reused storage')
                snapshots['before_release'] = _snapshot(log, 'before_release')
        snapshots['completed'] = _snapshot(log, 'completed')
    except Exception as exc:
        failures.append(f'{type(exc).__name__}: {exc}')
        log.event('worker_error', error_type=type(exc).__name__, error_message=str(exc))
    finally:
        for held, _ in holds:
            try:
                held.release()
            except Exception as exc:
                failures.append(f'hold release: {type(exc).__name__}: {exc}')
        if client is not None:
            try:
                cc.close(client)
                cleanup['close'] = 'ok'
            except Exception as exc:
                failures.append(f'client close: {type(exc).__name__}: {exc}')
        try:
            outcome = cc.shutdown(timeout=config['child_timeout'])
            cleanup['shutdown'] = outcome
            if not outcome['completed']:
                failures.append('client shutdown did not complete')
            snapshots['cleanup'] = _snapshot(log, 'cleanup')
        except Exception as exc:
            failures.append(f'shutdown: {type(exc).__name__}: {exc}')
        log.event('closed', cleanup=cleanup)
        log.close()
    return {
        'role': role, 'worker_index': spec['index'], 'pid': os.getpid(),
        'native_path': provenance['module_path'], 'native_sha256': provenance['sha256'],
        'provenance': provenance, 'config': config, 'summary': summarize_samples(log.samples),
        'by_phase': {phase: summarize_samples(log.samples, phase) for phase in ('before', 'during', 'after')},
        'samples_path': str(log.samples_path), 'events_path': str(log.events_path),
        'snapshots': snapshots, 'internal_probes': INTERNAL_PROBES,
        'cleanup': cleanup, 'failures': failures, 'ok': not failures,
    }


def budget_usage(snapshot: dict) -> dict:
    stats = snapshot['memory']['stats']
    result = {}
    for scope in [stats['runtime_outgoing'], stats['server'], *stats['retired']]:
        if scope is not None:
            for cell, value in scope['cells'].items():
                key = scope['role'] + '/' + cell
                result[key] = result.get(key, 0) + value['used_bytes']
    return result


def convergence(baseline: dict, current: dict) -> dict:
    before, after = budget_usage(baseline), budget_usage(current)
    budget_ok = all(value <= before.get(key, 0) for key, value in after.items())
    os_checks = {}
    for key in ('fd_count', 'handle_count', 'mapping_count'):
        a, b = baseline['os'][key]['value'], current['os'][key]['value']
        os_checks[key] = {'available': a is not None and b is not None,
                          'before': a, 'after': b,
                          'converged': b <= a if a is not None and b is not None else None}
    return {'budget_before': before, 'budget_after': after, 'budget_converged': budget_ok,
            'os': os_checks, 'converged': budget_ok and all(value['converged'] is not False for value in os_checks.values()),
            'scope': 'budget cells and available OS counts; unavailable counters are unmeasured',
            'rss_note': 'RSS and its high-water mark are recorded, not equated with backing refunds'}


def _finish_child(proc, timeout: float, log: SampleLog) -> dict:
    stdout, stderr = proc.communicate(timeout=timeout)
    code = proc.wait(timeout=timeout)
    log.event('os_wait', pid=proc.pid, exit_code=code)
    output = Path(log.events_path).parent
    (output / f'child-{proc.pid}.stdout.json').write_text(stdout, encoding='utf-8')
    (output / f'child-{proc.pid}.stderr.log').write_text(stderr, encoding='utf-8')
    value = json.loads(stdout)
    if not isinstance(value, dict):
        raise RuntimeError(f'child {proc.pid} returned no object; exit={code}')
    value['exit_code'] = code
    if code != 0:
        value['ok'] = False
        value.setdefault('failures', []).append(f'process exit {code}; stderr={stderr[-2000:]}')
    return value


def _cell_peak(worker: dict, phase: str, cell: str) -> int:
    stats = worker['snapshots'][phase]['memory']['stats']
    scope = stats['runtime_outgoing']
    return scope['cells'][cell]['peak_bytes'] if scope is not None else 0


def run_anomaly(args: argparse.Namespace) -> dict:
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    coord = output / 'coord'
    coord.mkdir(mode=0o700)
    endpoint_root = tempfile.mkdtemp(prefix='c2-bench-') if os.name != 'nt' else None
    if endpoint_root is not None:
        os.environ['C2_IPC_ROOT'] = endpoint_root
    os.environ['C2_ENV_FILE'] = ''
    os.environ['C2_RELAY_ANCHOR_ADDRESS'] = ''
    import c_two as cc
    from c_two import _native
    config = scenario_spec(args)
    provenance = dict(bench.runtime_provenance(_native), source_commit=bench.source_commit())
    log = SampleLog(output, {'role': 'controller', 'pid': os.getpid(), 'worker_index': -1}, config, provenance)
    failures, children, workers, snapshots, process_points = [], [], [], {}, []
    suspended = False
    abnormal = None
    receipt = None
    last_snapshot = 0.0
    def snapshot(phase):
        value = _snapshot(log, phase)
        snapshots[phase] = value
        for child in children:
            if child.poll() is None:
                process_points.append({'phase': phase, 'monotonic_ns': time.monotonic_ns(),
                                       'pid': child.pid, 'os': process_snapshot(child.pid)})
        return value
    def tick():
        nonlocal last_snapshot
        if time.monotonic() - last_snapshot >= 0.05:
            snapshot('progress-' + str(len(snapshots)))
            last_snapshot = time.monotonic()
    try:
        cc.set_transport_policy(shm_threshold=config['shm_threshold'])
        cc.set_server(ipc_overrides=config['server_overrides'])
        Exchange, Resource = workload_types(cc, coord, config)
        cc.register(Exchange, Resource(), name='exchange', concurrency=cc.ConcurrencyConfig(**config['route_concurrency']))
        address = cc.server_address()
        snapshot('registered')
        for index, role in enumerate(('normal', 'abnormal') if args.control == 'parallel' else ('normal',)):
            spec = {'role': role, 'index': index, 'address': address, 'coord_dir': str(coord),
                    'output_dir': str(output), 'config': config}
            child = subprocess.Popen(
                [args.python, str(Path(bench.__file__).resolve()), '--python', args.python,
                 '--anomaly-worker-spec', json.dumps(spec)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            children.append(child)
            if role == 'abnormal':
                abnormal = child
        bench.release_workers(coord, children, args.child_timeout, log)
        snapshot('ready')
        publish(coord / 'normal-before.json')
        wait_marker(coord / 'normal-before-done.json', args.child_timeout, children, tick)
        snapshots['baseline'] = snapshot('before')
        if abnormal is not None:
            publish(coord / 'abnormal-start.json')
            if args.scenario in GATED:
                entered = wait_for_entered(coord, config['inflight_calls'], args.child_timeout, children, tick)
                log.event('resource_gated', calls=config['inflight_calls'], entered=entered)
                if args.scenario in STOPPED:
                    suspend_process(abnormal)
                    suspended = True
                    if os.name != 'nt':
                        waited, status = os.waitpid(abnormal.pid, os.WUNTRACED)
                        if waited != abnormal.pid or not os.WIFSTOPPED(status):
                            raise RuntimeError('OS did not confirm the owned reader stopped')
                    log.event('reader_stopped', pid=abnormal.pid, mechanism='native_suspend' if os.name == 'nt' else 'SIGSTOP')
                    publish(coord / 'method-release.json')
                    for index in range(config['inflight_calls']):
                        wait_marker(coord / f'returning-abnormal-{index}.json', args.child_timeout, children, tick)
                    deadline = time.monotonic() + args.pause_seconds
                    while time.monotonic() < deadline:
                        tick()
                        time.sleep(0.005)
                else:
                    log.event('slow_method_waiting')
            else:
                wait_marker(coord / 'abnormal-active.json', args.child_timeout, children, tick)
            log.event('abnormal_ready', scenario=args.scenario)
        else:
            log.event('alone_control_ready', scenario=args.scenario)
        snapshot('interference_ready')
        publish(coord / 'normal-during.json')
        log.event('normal_released', phase='during')
        wait_marker(coord / 'normal-during-done.json', args.child_timeout, children, tick)
        snapshot('during')
        if abnormal is not None:
            if suspended:
                resume_process(abnormal)
                suspended = False
                log.event('reader_resumed', pid=abnormal.pid)
            publish(coord / 'method-release.json')
            publish(coord / 'abnormal-stop.json')
            workers.append(_finish_child(abnormal, args.child_timeout, log))
            snapshot('abnormal_closed')
        publish(coord / 'normal-after.json')
        # The normal process exits after completing its after phase; a marker
        # can race process exit, so this wait deliberately checks only time.
        wait_marker(coord / 'normal-after-done.json', args.child_timeout, tick=tick)
        workers.insert(0, _finish_child(children[0], args.child_timeout, log))
        for worker in workers:
            if not worker['ok']:
                failures.extend(f"{worker['role']}: {failure}" for failure in worker['failures'])
        snapshot('clients_closed')
        deadline = time.monotonic() + min(args.child_timeout, 15.0)
        while True:
            current = snapshot('convergence')
            receipt = convergence(snapshots['baseline'], current)
            if receipt['converged'] or time.monotonic() >= deadline:
                break
            time.sleep(0.05)
        if not receipt['converged']:
            failures.append('backing or available OS counts did not converge after release/close')
        normal = workers[0]
        if normal['summary']['attempts'] != 3 * args.calls:
            failures.append('normal client has missing raw attempts')
        # Standalone runs enforce the same on-disk evidence contract as paired
        # runs, including identity, timestamps, quantiles and complete counts.
        from ipc_paired import check_worker
        for worker in workers:
            expected = 3 * args.calls if worker['role'] == 'normal' else (
                config['inflight_calls'] if args.scenario in GATED else config['abnormal_calls']
            )
            if worker['role'] == 'abnormal' and args.scenario == 'release_busy':
                if worker['summary']['attempts'] < expected:
                    failures.append('busy client has missing raw attempts')
                expected = None
            check_worker(worker, output, False, provenance['source_commit'], expected, config)
        if args.control == 'parallel' and args.scenario.startswith('fallback_'):
            if snapshots['interference_ready']['memory']['stats']['server']['cells']['shm']['used_bytes'] < MIB:
                failures.append('stopped responses did not create the required full buddy backing')
            if args.scenario == 'fallback_dedicated':
                peak = snapshots['during']['memory']['stats']['server']['cells']['shm']['peak_bytes']
                if peak <= MIB:
                    failures.append('no dedicated SHM backing beyond the full buddy segment')
            if args.scenario == 'fallback_inline' and _cell_peak(normal, 'during', 'reassembly') == 0:
                failures.append('no receive reassembly observed for the forced chunk fallback')
            if args.scenario == 'fallback_file' and _cell_peak(normal, 'during', 'file') == 0:
                failures.append('no receiver file backing observed for forced file reassembly')
        expected_hash = provenance['sha256']
        if not expected_hash or not provenance['module_path'] or not provenance['source_commit']:
            failures.append('controller native provenance missing')
        if any(worker['native_sha256'] != expected_hash for worker in workers):
            failures.append('process native hashes disagree')
    except Exception as exc:
        failures.append(f'{type(exc).__name__}: {exc}')
        log.event('controller_error', error_type=type(exc).__name__, error_message=str(exc), traceback=traceback.format_exc()[-2000:])
    finally:
        if suspended and abnormal is not None and abnormal.poll() is None:
            try:
                resume_process(abnormal)
                log.event('reader_resumed_for_cleanup', pid=abnormal.pid)
            except OSError as exc:
                failures.append(f'cleanup resume: {exc}')
        publish(coord / 'method-release.json')
        publish(coord / 'abnormal-stop.json')
        for child in children:
            if child.poll() is None:
                note = bench.kill_owned_child(child)
                failures.append(f'forced termination of owned child {child.pid}: {note or "reaped"}')
                log.event('os_wait', pid=child.pid, exit_code=child.returncode, forced=True)
        try:
            shutdown = cc.shutdown(timeout=args.child_timeout)
            if not shutdown['completed']:
                failures.append('controller shutdown did not complete')
            snapshot('shutdown')
            if any(budget_usage(snapshots['shutdown']).values()):
                failures.append('backing/reassembly budget remains charged after shutdown')
        except Exception as exc:
            failures.append(f'controller shutdown: {type(exc).__name__}: {exc}')
        log.event('closed')
        log.close()
    report = {
        'schema': 'c-two.ipc-anomaly.v1', 'scenario': args.scenario, 'control': args.control,
        'config': config, 'source_commit': provenance['source_commit'], 'endpoint_root': endpoint_root,
        'benchmark': {'workers': workers, 'server': {'pid': os.getpid(), 'provenance': provenance,
                       'native_path': provenance['module_path'], 'native_sha256': provenance['sha256']},
                      'events_path': str(log.events_path)},
        'snapshots': snapshots, 'process_points': process_points, 'convergence': receipt,
        'internal_probes': INTERNAL_PROBES, 'existing_release_tests': RELEASE_TESTS,
        'release_tests_executed_by_this_tool': False,
        'blocked_frame_observation': 'reader stopped before resource returned; exact bytes written are unmeasured; no control-frame bypass claim' if args.scenario == 'release_blocked' else None,
        'isolation_success_eligible': args.scenario != 'exhausted',
        'performance_thresholds': None, 'failures': failures, 'ok': not failures,
    }
    (output / 'anomaly.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    return report
