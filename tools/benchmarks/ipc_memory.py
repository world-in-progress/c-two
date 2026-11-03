"""Measure isolated, real IPC calls without treating mapping capacity as RSS.

Modes:
- single (default): one controller process. By default, worker processes
  connect to one shared server in the controller; --topology pairs gives
  each worker its own server. Explicit ipc:// addresses exclude direct
  dispatch. Run separate invocations for cold/small and large payloads.
- matrix: runs a fixed, laptop-sized row set covering the approved memory
  plan's four workload classes (idle/small RPC, a four-worker burst against
  one shared server, multi-MiB science-like payloads, deterministic forced
  budget fallback/exhaustion) under default-buddy and buddy-disabled
  configurations. Each row runs in its own controller subprocess; artifacts
  are written under --output-dir.
- anomaly: runs one alone/parallel control with before/during/after phases.
  See --scenario. ipc_paired.py alternates separate baseline/candidate Python
  environments for at least five groups, reporting performance thresholds.

Every worker flushes per-attempt samples and events to JSONL. Each sample
file starts with run configuration, commit and the imported native hash.
Warmups and hold-validation calls are retained but excluded from measured
quantiles. P50/P95/P99 use nearest rank over all measured attempts, including
errors and timeouts. Throughput counts successful calls over the measurement
wall envelope, including between-call harness overhead. Workers connect and
warm up before the controller publishes one common barrier timestamp.

Expected-capacity-failure rows (--expect-error-substring) require the CRM
call to raise an error containing the substring; the stable native phrase is
"memory budget cell". Success in that mode fails validation, a raised error
without the substring is reported as the wrong error, and an error raised
while connecting is reported as the wrong stage even when the substring and
the control ping both match: only a call-stage capacity failure is accepted.
The exact observed error text, its stage, and the ping result stay in the
artifact. After the expected failure the worker proves the direct IPC control
ping still works and then performs normal client/server cleanup.

Only processes the driver itself spawned are ever terminated: worker joins
are bounded by --child-timeout, row joins by --row-timeout, and a forced
termination is always reported as a failure instead of clean shutdown. A
timed-out matrix row signals the session that row controller leads, never an
unrelated process. Every post-kill drain is bounded and closes its pipes, so
a descendant holding an inherited pipe is reported as a drain failure
instead of being waited on forever. --workers is capped at MAX_WORKERS and
both timeouts must be finite positive values no greater than
MAX_CHILD_TIMEOUT_S / MAX_ROW_TIMEOUT_S, rejected before any process spawns.
A worker writes its JSON report to stdout and exits 0 only when it produced a
complete report; otherwise it exits 1, so a validation failure stays
distinguishable from a missing report.

OS current/peak RSS includes Python and serialization; it is not an allocator
byte counter or backing-capacity measurement. RSS, backing budgets and local
hold counts are separate fields. Unavailable OS counters are null with a
reason. Internal loan/queue probes are explicitly unmeasured. Runtime
memory snapshots use the public c_two.observe.memory_stats API; snapshot errors
are recorded, and --require-memory-stats fails instead of fabricating zero
values. In require mode every advertised
snapshot point is enforced, including after cleanup. Post-shutdown
unavailability can only be exempted explicitly through
--allow-unavailable-stats-after-shutdown, which records the exemption inside
the snapshot itself; it never silently passes.

Every result records the module path and SHA-256 of the native extension that
was actually imported plus the repository HEAD and tracked-file dirtiness of
the tree that owns it. That is provenance for the imported binary, not proof
that it was built from the harness's or a worker's Git HEAD, and a missing
path or hash is a provenance failure rather than an empty-set hash agreement.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.metadata
import json
import math
import os
from pathlib import Path
import signal
import statistics
import subprocess
import sys
import tempfile
import time
import traceback

if str(Path(__file__).resolve().parent) not in sys.path:
    sys.path.insert(0, str(Path(__file__).resolve().parent))

from ipc_evidence import (
    INTERNAL_PROBES, CORRECTNESS_FAILURE, Failures, SampleLog,
    TOOL_FAILURE, failure_message, platform_process_support, process_snapshot,
    publish_marker, read_marker, summarize_samples,
)


BUDGET_ERROR_PHRASE = 'memory budget cell'

# Laptop-sized bounds. A worker is a real OS process, so every join and every
# post-kill drain must terminate; the matrix driver refuses values outside
# these ranges before it spawns anything.
MAX_WORKERS = 16
MAX_CHILD_TIMEOUT_S = 600.0
MAX_ROW_TIMEOUT_S = 3600.0
POST_KILL_DRAIN_TIMEOUT_S = 30.0

# Primary buddy geometry only. It caps the request/response pool and does not
# bound the separate chunk-reassembly buddy pool or dedicated SHM mappings.
PRIMARY_MAX_POOL_SEGMENTS = 2
# Small reassembly geometry for laptop rows: the native defaults are
# 64 MiB x 4, which no row here needs.
REASSEMBLY_SEGMENT_BYTES = 8 * 1024 * 1024
REASSEMBLY_MAX_SEGMENTS = 1


def peak_rss_bytes() -> int | None:
    try:
        import resource
    except ImportError:
        return None
    rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return int(rss if sys.platform == 'darwin' else rss * 1024)


def payload_pattern(tag: str, size: int) -> bytes:
    """Deterministic content so transposed or truncated transfers fail the equality check."""
    block = bytearray()
    counter = 0
    while len(block) < 4096:
        block += hashlib.sha256(f'{tag}:{counter}'.encode()).digest()
        counter += 1
    base = bytes(block)
    repeats = -(-size // len(base))
    return (base * repeats)[:size]


def native_digest(path: str) -> str:
    digest = hashlib.sha256()
    with Path(path).open('rb') as native_file:
        for block in iter(lambda: native_file.read(1 << 20), b''):
            digest.update(block)
    return digest.hexdigest()


def distribution_version(name: str) -> str | None:
    try:
        return importlib.metadata.version(name)
    except Exception:
        return None


def git_output(*arguments: str, cwd: Path) -> str | None:
    try:
        completed = subprocess.run(
            ['git', *arguments], capture_output=True, text=True, timeout=30, cwd=cwd,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if completed.returncode != 0:
        return None
    return completed.stdout.rstrip()


def repo_state(path: Path) -> dict | None:
    """Record HEAD and tracked-file dirtiness for the tree that owns a file.

    Never presented as the commit that built the file: a dirty tree means the
    recorded HEAD may not describe the imported binary at all.
    """
    root = git_output('rev-parse', '--show-toplevel', cwd=path.resolve().parent)
    if not root:
        return None
    status = git_output('status', '--porcelain', '--untracked-files=no', cwd=Path(root))
    dirty_paths = None if status is None else [line[3:] for line in status.splitlines() if line.strip()]
    return {
        'root': root,
        'head': git_output('rev-parse', 'HEAD', cwd=Path(root)),
        'tracked_dirty': None if dirty_paths is None else bool(dirty_paths),
        'tracked_dirty_paths': None if dirty_paths is None else dirty_paths[:10],
    }


def runtime_provenance(native_module) -> dict:
    """Actual import path, hash, and repository state of the loaded native extension."""
    source = getattr(native_module, '__file__', None)
    provenance = {'module_path': source, 'sha256': None, 'repo': None}
    if source and Path(source).is_file():
        provenance['sha256'] = native_digest(source)
        provenance['repo'] = repo_state(Path(source))
    return provenance


def source_commit() -> str | None:
    root = os.environ.get('C2_BENCH_SOURCE_ROOT')
    state = repo_state(Path(root) / 'pyproject.toml' if root else Path(__file__))
    return state['head'] if state else None


def echo_types(cc):
    @cc.crm(namespace='c_two.benchmark.memory', version='1.0.0')
    class Echo:
        def echo(self, value: bytes) -> bytes: ...

    class EchoResource:
        def echo(self, value: bytes) -> bytes:
            return value

    return Echo, EchoResource


def configure(cc, args: argparse.Namespace) -> dict:
    """Pin laptop-sized primary and chunk-reassembly buddy geometry.

    `max_pool_segments` caps the primary request/response buddy pool only. It
    does not bound the separate chunk-reassembly buddy pool (native default
    64 MiB x 4) or dedicated SHM mappings, which are per-allocation. The
    reassembly geometry is therefore pinned explicitly here and can still be
    overridden through --ipc-overrides.
    """
    overrides = {
        'pool_enabled': args.pool_enabled,
        'pool_segment_size': args.segment_size,
        'max_pool_segments': PRIMARY_MAX_POOL_SEGMENTS,
        'reassembly_segment_size': REASSEMBLY_SEGMENT_BYTES,
        'reassembly_max_segments': REASSEMBLY_MAX_SEGMENTS,
    }
    overrides.update(args.ipc_overrides)
    cc.set_transport_policy(shm_threshold=args.shm_threshold)
    cc.set_server(ipc_overrides=overrides)
    cc.set_client(ipc_overrides=overrides)
    return overrides


def snapshot_memory_stats() -> dict:
    """One runtime snapshot through the public API, or an explicit error record."""
    from c_two.observe import memory_stats

    try:
        return {'available': True, 'stats': common_memory_stats(dict(memory_stats()))}
    except Exception as exc:  # recorded honestly, never replaced with zeros
        return {'available': False, 'error': f'{type(exc).__name__}: {exc}'}


def common_memory_stats(stats: dict) -> dict:
    """The same current public fields on both inputs; no retired counters."""
    def scope(value):
        if value is None:
            return None
        return {key: value[key] for key in ('role', 'state', 'limits', 'cells')}
    return {
        'runtime_outgoing': scope(stats['runtime_outgoing']),
        'server': scope(stats['server']),
        'retired': [scope(value) for value in stats['retired']],
        'holds': {key: stats['holds'][key] for key in (
            'active_holds', 'total_held_bytes', 'oldest_hold_seconds', 'by_direction',
        )},
    }


def wait_for_release(coord: Path, worker_index: int, timeout: float, log: SampleLog) -> dict:
    ready = {'pid': os.getpid(), 'worker_index': worker_index, 'ready_ns': time.perf_counter_ns()}
    publish_marker(coord / f'ready-{worker_index}.json', ready)
    log.event('ready', **ready)
    deadline = time.perf_counter() + timeout
    while (release := read_marker(coord / 'release.json')) is None:
        if time.perf_counter() >= deadline:
            raise TimeoutError('controller did not release the ready barrier')
        time.sleep(0.005)
    log.event('barrier_observed', **release)
    return release


def release_workers(coord: Path, children: list, timeout: float, log: SampleLog) -> dict:
    deadline = time.perf_counter() + timeout
    while True:
        workers = [read_marker(coord / f'ready-{index}.json') for index in range(len(children))]
        if all(worker is not None for worker in workers):
            break
        if any(child.poll() is not None for child in children):
            raise RuntimeError('worker exited before the ready barrier')
        if time.perf_counter() >= deadline:
            raise TimeoutError('workers did not finish connection and warmup')
        time.sleep(0.005)
    release = {'release_ns': time.perf_counter_ns(), 'worker_count': len(children), 'workers': workers}
    # Rename publishes a complete timestamp to every waiter at once.
    publish_marker(coord / 'release.json', release)
    log.event('barrier_released', **release)
    return release


def call_sample(log: SampleLog, request_id: int, call, payload: bytes, *, phase='measurement', measurement=True):
    log.event('call_started', request_id=request_id, phase=phase)
    start = time.perf_counter_ns()
    status, error_type, error_message, integrity = 'success', None, None, None
    value = None
    try:
        value = call(payload)
        integrity = value == payload
        if not integrity:
            raise ValueError('IPC payload mismatch')
    except Exception as exc:
        timed_out = (
            isinstance(exc, TimeoutError)
            or type(exc).__name__ in ('CallDeadlineExceeded', 'TimeoutError')
            or getattr(exc, 'name', None) == 'CallDeadlineExceeded'
            or getattr(exc, 'io_kind', None) == 'TimedOut'
        )
        status = 'timeout' if timed_out else 'error'
        error_type, error_message = type(exc).__name__, str(exc)
    sample = log.sample(
        request_id, start, time.perf_counter_ns(), status, error_type, error_message,
        phase=phase, integrity_ok=integrity, measurement=measurement,
    )
    return value, sample


def memory_stats_usable(snapshot: dict) -> bool:
    return bool(snapshot.get('available')) and 'stats' in snapshot and 'error' not in snapshot


def require_no_charges(snapshot: dict, point: str, failures: list[str]) -> None:
    if not memory_stats_usable(snapshot):
        return  # require_snapshot owns unavailable-snapshot enforcement.
    stats = snapshot['stats']
    scopes = [stats['runtime_outgoing'], stats['server'], *stats['retired']]
    charged = [
        f"{scope['role']}/{name}={cell['used_bytes']}"
        for scope in scopes if scope is not None
        for name, cell in scope['cells'].items() if cell['used_bytes']
    ]
    if charged or stats['holds']['active_holds']:
        failures.append(f'{point}: cleanup retains charges or local holds: {charged}')


def require_snapshot(
    snapshot: dict,
    point: str,
    failures: list[str],
    *,
    allow_unavailable_after_shutdown: bool = False,
) -> None:
    """Enforce one advertised --require-memory-stats snapshot point.

    Every point is required, including after cleanup. The only exemption is
    the explicit --allow-unavailable-stats-after-shutdown contract, and it is
    written into the snapshot itself so the artifact never looks complete by
    accident.
    """
    if memory_stats_usable(snapshot):
        return
    if allow_unavailable_after_shutdown:
        snapshot['contract'] = (
            'post-shutdown unavailability explicitly exempted by '
            '--allow-unavailable-stats-after-shutdown; not a fabricated snapshot'
        )
        return
    failures.append(failure_message(f'memory stats required but unavailable at {point}', TOOL_FAILURE))


def measure(args: argparse.Namespace) -> dict:
    os.environ['C2_ENV_FILE'] = ''
    os.environ['C2_RELAY_ANCHOR_ADDRESS'] = ''
    import c_two as cc
    from c_two import _native

    failures = Failures()
    cleanup: dict[str, object] = {}
    Echo, EchoResource = echo_types(cc)
    overrides = configure(cc, args)
    provenance = dict(runtime_provenance(_native), source_commit=source_commit())
    identity = {'role': 'normal', 'pid': os.getpid(), 'worker_index': args.worker_index}
    log = SampleLog(args.output_dir, identity, vars_for_config(args), provenance)
    stats_enabled = args.memory_stats or args.require_memory_stats
    memory_stats: dict[str, dict] = {}
    os_stats = {'before_connect': process_snapshot()}
    expected = args.expect_error_substring
    expected_error = None if expected is None else {
        'required_substring': expected, 'required_stage': 'call', 'raised': False,
        'stage': None, 'stage_matched': None, 'error_type': None,
        'error_message': None, 'observed': None, 'control_ping_ok': None, 'matched': None,
    }
    payload = payload_pattern('c-two-ipc-memory', args.payload_bytes)
    client = None
    connected_ns = None
    held_stats = after_release = barrier = None
    started = time.perf_counter_ns()
    try:
        if stats_enabled:
            memory_stats['before_connect'] = snapshot_memory_stats()
        if args.address is None:
            cc.register(Echo, EchoResource(), name='echo')
        address = args.address or cc.server_address()
        try:
            client = cc.connect(Echo, name='echo', address=address, timeout=args.call_timeout)
        except Exception as exc:
            if expected_error is not None:
                expected_error.update(raised=True, stage='connect', error_type=type(exc).__name__, error_message=str(exc))
            raise
        connected_ns = time.perf_counter_ns() - started
        # One public deadline API is used on both source inputs.
        proxy = cc.with_call_options(client, timeout=args.call_timeout)
        request_id = 0
        if expected is None:
            for _ in range(args.warmup):
                _, sample = call_sample(log, request_id, proxy.echo, payload, phase='warmup', measurement=False)
                request_id += 1
                if sample['status'] != 'success':
                    raise RuntimeError(f"warmup failed: {sample['error_type']}: {sample['error_message']}")
        barrier = wait_for_release(args.coord_dir, args.worker_index, args.child_timeout, log)
        for _ in range(args.calls):
            _, sample = call_sample(log, request_id, proxy.echo, payload)
            request_id += 1
            if expected_error is not None and sample['status'] == 'success':
                failures.setup(f"request {sample['request_id']}: expected capacity error but call succeeded")
            if sample['status'] != 'success':
                if expected_error is None:
                    failures.append(f"request {sample['request_id']}: {sample['error_type']}: {sample['error_message']}")
                else:
                    observed = f"{sample['error_type']}: {sample['error_message']}"
                    expected_error.update(
                        raised=True, stage='call', stage_matched=True,
                        error_type=sample['error_type'], error_message=sample['error_message'],
                        observed=observed, matched=expected in observed,
                    )
                    if expected not in observed:
                        failures.append(f'wrong capacity error: {observed}')
        if expected_error is not None:
            if not expected_error['raised']:
                failures.setup('expected native capacity error was not raised; the call succeeded')
            if expected_error['stage'] != 'call':
                failures.setup('expected capacity failure at call stage')
            from c_two.transport.client import util
            expected_error['control_ping_ok'] = bool(util.ping(address, timeout=2.0))
            if not expected_error['control_ping_ok']:
                failures.append('direct IPC control ping failed after capacity error')
        else:
            # Validate hold without mixing this additional call into measured RPCs.
            held_box = []
            def held_call(value):
                held_box.append(cc.hold(proxy.echo)(value))
                return held_box[-1].value
            try:
                _, sample = call_sample(log, request_id, held_call, payload, phase='hold_validation', measurement=False)
                if sample['status'] != 'success':
                    failures.append(f"hold validation: {sample['error_message']}")
                held_stats = snapshot_memory_stats()['stats']['holds']
            finally:
                for held in held_box:
                    held.release()
            after_release = snapshot_memory_stats()['stats']['holds']
            if after_release['active_holds'] != 0:
                failures.append('benchmark retained a local hold')
        if stats_enabled:
            memory_stats['after_calls'] = snapshot_memory_stats()
        os_stats['after_calls'] = process_snapshot()
    except Exception as exc:
        failures.append(f'{type(exc).__name__}: {exc}', getattr(exc, 'category',
            TOOL_FAILURE if isinstance(exc, (TimeoutError, OSError)) else CORRECTNESS_FAILURE))
        log.event('worker_error', error_type=type(exc).__name__, error_message=str(exc))
    finally:
        if client is not None:
            try:
                cc.close(client)
                cleanup['close_client'] = 'ok'
            except Exception as exc:
                cleanup['close_client'] = f'{type(exc).__name__}: {exc}'
                failures.append('client close failed after the run')
        try:
            shutdown = cc.shutdown(timeout=args.child_timeout)
            cleanup['shutdown'] = shutdown
            if not shutdown['completed']:
                failures.append('cc.shutdown did not complete')
        except Exception as exc:
            cleanup['shutdown'] = f'{type(exc).__name__}: {exc}'
            failures.append('cc.shutdown failed after the run')
        if stats_enabled:
            memory_stats['after_cleanup'] = snapshot_memory_stats()
            require_no_charges(memory_stats['after_cleanup'], 'worker after_cleanup', failures)
        if args.require_memory_stats:
            for name in ('before_connect', 'after_calls', 'after_cleanup'):
                require_snapshot(
                    memory_stats.get(name, {}), name + ' (worker)', failures,
                    allow_unavailable_after_shutdown=name == 'after_cleanup' and args.allow_unavailable_stats_after_shutdown,
                )
        os_stats['after_cleanup'] = process_snapshot()
        log.event('closed', cleanup=cleanup)
        log.close()
    summary = summarize_samples(log.samples)
    if summary['attempts'] != args.calls:
        failures.tool(f"sample count mismatch: expected {args.calls}, measured {summary['attempts']}")
    if not provenance['module_path'] or not provenance['sha256'] or not provenance['source_commit']:
        failures.tool('native path/hash or source commit missing')
    return {
        'pid': os.getpid(), 'role': 'normal', 'worker_index': args.worker_index,
        'python_executable': sys.executable, 'version': distribution_version('c-two'),
        'fastdb_version': distribution_version('fastdb4py'), 'python': sys.version,
        'platform': sys.platform, 'argv': list(sys.argv), 'overrides': overrides,
        'native_path': provenance['module_path'], 'native_sha256': provenance['sha256'],
        'native_repo': provenance['repo'], 'provenance': provenance,
        'config': vars_for_config(args), 'barrier': barrier, 'connect_ns': connected_ns,
        'summary': summary, 'samples_path': str(log.samples_path), 'events_path': str(log.events_path),
        'held': held_stats, 'after_release': after_release, 'os_stats': os_stats,
        'memory_stats': memory_stats, 'internal_probes': INTERNAL_PROBES,
        'expected_error': expected_error, 'cleanup': cleanup, 'failures': failures, 'ok': not failures,
    }


def vars_for_config(args: argparse.Namespace) -> dict:
    return {key: getattr(args, key) for key in (
        'pool_enabled', 'segment_size', 'payload_bytes', 'shm_threshold', 'calls',
        'ipc_overrides', 'expect_error_substring', 'warmup', 'call_timeout',
    )}


def run_worker(args: argparse.Namespace) -> dict:
    try:
        return measure(args)
    except Exception as exc:
        return {
            'ok': False,
            'pid': os.getpid(),
            'python_executable': sys.executable,
            'platform': sys.platform,
            'error_type': type(exc).__name__,
            'error_message': str(exc),
            'traceback_tail': traceback.format_exc()[-2000:],
            'failures': [failure_message(f'{type(exc).__name__}: {exc}', TOOL_FAILURE)],
        }


def drain_killed_child(proc: subprocess.Popen, timeout: float = POST_KILL_DRAIN_TIMEOUT_S) -> str | None:
    """Finish one killed, owned child and always close its pipes.

    The drain is bounded: if a descendant inherited the stdout/stderr pipes,
    they are closed here and reported as a drain failure instead of being
    waited on indefinitely.
    """
    try:
        proc.communicate(timeout=timeout)
        return None
    except subprocess.TimeoutExpired:
        pass
    for stream in (proc.stdout, proc.stderr):
        if stream is not None:
            try:
                stream.close()
            except OSError:
                pass
    try:
        proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        return (
            f'process {proc.pid} did not exit within {timeout}s of its kill and its stdout/stderr '
            'pipes stayed open; the pipes were closed without a full drain'
        )
    return (
        f'process {proc.pid} exited but its stdout/stderr pipes stayed open for {timeout}s after '
        'the kill (a descendant may still hold them); the pipes were closed'
    )


def kill_owned_child(proc: subprocess.Popen) -> str | None:
    """Kill exactly this owned child process (never its group) and bound the drain."""
    if proc.poll() is None:
        proc.kill()
    return drain_killed_child(proc)


def run_controller(args: argparse.Namespace) -> dict:
    args.output_dir = Path(args.output_dir or tempfile.mkdtemp(prefix='c2-ipc-evidence-')).resolve()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    args.coord_dir = args.output_dir / 'coord'
    args.coord_dir.mkdir(mode=0o700)
    endpoint_root = tempfile.mkdtemp(prefix='c2-bench-') if os.name != 'nt' else None
    if endpoint_root is not None:
        os.environ['C2_IPC_ROOT'] = endpoint_root
    controller_log = SampleLog(
        args.output_dir, {'role': 'controller', 'pid': os.getpid(), 'worker_index': -1},
        vars_for_config(args), {'source_commit': source_commit()},
    )
    command = [
        args.python, str(Path(__file__).resolve()), '--worker',
        '--pool-enabled' if args.pool_enabled else '--no-pool-enabled',
        '--segment-size', str(args.segment_size),
        '--payload-bytes', str(args.payload_bytes),
        '--shm-threshold', str(args.shm_threshold), '--calls', str(args.calls),
        '--ipc-overrides', json.dumps(args.ipc_overrides),
        '--output-dir', str(args.output_dir), '--coord-dir', str(args.coord_dir),
        '--warmup', str(args.warmup), '--call-timeout', str(args.call_timeout),
        '--child-timeout', str(args.child_timeout),
    ]
    if args.expect_error_substring is not None:
        command.extend(['--expect-error-substring', args.expect_error_substring])
    if args.memory_stats:
        command.append('--memory-stats')
    if args.require_memory_stats:
        command.append('--require-memory-stats')
    if args.allow_unavailable_stats_after_shutdown:
        command.append('--allow-unavailable-stats-after-shutdown')

    children: list[subprocess.Popen] = []
    results = []
    child_processes: list[dict] = []
    terminated: list[int] = []
    failures = Failures()
    cleanup: dict[str, object] = {}
    server_cc = None
    server_stats = None
    server_memory_stats: dict[str, dict] = {}
    stats_enabled = args.memory_stats or args.require_memory_stats
    started = time.perf_counter_ns()
    barrier = None
    try:
        if args.topology == 'shared':
            os.environ['C2_ENV_FILE'] = ''
            os.environ['C2_RELAY_ANCHOR_ADDRESS'] = ''
            import c_two as server_cc

            Echo, EchoResource = echo_types(server_cc)
            configure(server_cc, args)
            server_cc.register(Echo, EchoResource(), name='echo')
            command.extend(['--address', server_cc.server_address()])
            from c_two import _native as server_native
            server_provenance = runtime_provenance(server_native)
            server_stats = {
                'pid': os.getpid(),
                'rss_after_register_high_water_bytes': peak_rss_bytes(),
                'native_path': server_provenance['module_path'],
                'native_sha256': server_provenance['sha256'],
                'native_repo': server_provenance['repo'],
                'provenance': dict(server_provenance, source_commit=source_commit()),
                'os_stats': {'after_register': process_snapshot()},
            }
            if stats_enabled:
                server_memory_stats['after_register'] = snapshot_memory_stats()
                if args.require_memory_stats:
                    require_snapshot(server_memory_stats['after_register'], 'after_register (server)', failures)
        for index in range(args.workers):
            children.append(subprocess.Popen(
                command + ['--worker-index', str(index)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            ))
        barrier = release_workers(args.coord_dir, children, args.child_timeout, controller_log)
        for child in children:
            try:
                stdout, stderr = child.communicate(timeout=args.child_timeout)
            except subprocess.TimeoutExpired:
                drain_note = kill_owned_child(child)
                terminated.append(child.pid)
                child_processes.append({
                    'pid': child.pid, 'exit_code': child.returncode,
                    'forced_termination': True, 'drain': drain_note or 'ok',
                })
                failures.tool(
                    f'benchmark child {child.pid} exceeded the {args.child_timeout}s join timeout '
                    'and was terminated; termination is reported as a failure'
                )
                if drain_note:
                    failures.tool(f'benchmark child {child.pid} post-kill drain failed: {drain_note}')
                continue
            child_processes.append({'pid': child.pid, 'exit_code': child.returncode, 'forced_termination': False})
            (args.output_dir / f'child-{child.pid}.stdout.json').write_text(stdout, encoding='utf-8')
            (args.output_dir / f'child-{child.pid}.stderr.log').write_text(stderr, encoding='utf-8')
            worker_result = None
            if stdout.strip():
                try:
                    worker_result = json.loads(stdout)
                except json.JSONDecodeError:
                    worker_result = None
            if child.returncode:
                detail = ''
                if isinstance(worker_result, dict):
                    detail = '; '.join(worker_result.get('failures') or [])
                if not detail:
                    detail = (stderr or '').strip()[-2000:]
                if isinstance(worker_result, dict) and worker_result.get('failures'):
                    failures.extend(worker_result['failures'])
                else:
                    failures.tool(f'benchmark child {child.pid} failed with exit code {child.returncode}: {detail}')
                if isinstance(worker_result, dict):
                    results.append(worker_result)
                continue
            if not isinstance(worker_result, dict):
                failures.tool(f'benchmark child {child.pid} produced no JSON result')
                continue
            if not worker_result.get('ok'):
                failures.extend(worker_result.get('failures') or [failure_message(
                    f'benchmark child {child.pid} reported failure without a reason', TOOL_FAILURE)])
            results.append(worker_result)
        if server_stats is not None:
            server_stats['rss_after_calls_high_water_bytes'] = peak_rss_bytes()
            server_stats['os_stats']['after_calls'] = process_snapshot()
            if stats_enabled:
                server_memory_stats['after_workers'] = snapshot_memory_stats()
                if args.require_memory_stats:
                    require_snapshot(server_memory_stats['after_workers'], 'after_workers (server)', failures)
    except Exception as exc:
        failures.append(f'controller: {type(exc).__name__}: {exc}', getattr(exc, 'category', TOOL_FAILURE))
        controller_log.event('controller_error', error_type=type(exc).__name__, error_message=str(exc))
    finally:
        for child in children:
            if child.poll() is None:
                drain_note = kill_owned_child(child)
                child_processes.append({
                    'pid': child.pid, 'exit_code': child.returncode,
                    'forced_termination': True, 'drain': drain_note or 'ok', 'stage': 'cleanup',
                })
                if child.pid not in terminated:
                    terminated.append(child.pid)
                    failures.tool(
                        f'benchmark child {child.pid} was still running during cleanup and was terminated'
                    )
                if drain_note:
                    failures.tool(f'benchmark child {child.pid} post-kill drain failed: {drain_note}')
            elif not any(record['pid'] == child.pid for record in child_processes):
                # A worker can fail before readiness. Retain its original
                # failure report instead of losing it behind a barrier error.
                try:
                    stdout, stderr = child.communicate(timeout=POST_KILL_DRAIN_TIMEOUT_S)
                except subprocess.TimeoutExpired:
                    note = drain_killed_child(child)
                    stdout, stderr = '', note or 'worker pipes stayed open after exit'
                    failures.tool(f'worker {child.pid} failure-report drain timed out: {stderr}')
                (args.output_dir / f'child-{child.pid}.stdout.json').write_text(stdout, encoding='utf-8')
                (args.output_dir / f'child-{child.pid}.stderr.log').write_text(stderr, encoding='utf-8')
                child_processes.append({'pid': child.pid, 'exit_code': child.returncode, 'forced_termination': False})
                try:
                    failed_worker = json.loads(stdout)
                    if isinstance(failed_worker, dict):
                        results.append(failed_worker)
                except json.JSONDecodeError:
                    failures.tool(f'worker {child.pid} exited without a JSON failure record')
        if server_cc is not None:
            try:
                shutdown = server_cc.shutdown(timeout=args.child_timeout)
                cleanup['server_shutdown'] = shutdown
                if not shutdown['completed']:
                    failures.append('controller shutdown did not complete')
            except Exception as exc:
                cleanup['server_shutdown'] = f'{type(exc).__name__}: {exc}'
                failures.append('controller server shutdown failed after the run')
            if stats_enabled:
                server_memory_stats['after_cleanup'] = snapshot_memory_stats()
                require_no_charges(server_memory_stats['after_cleanup'], 'server after_cleanup', failures)
                if args.require_memory_stats:
                    require_snapshot(
                        server_memory_stats['after_cleanup'], 'after_cleanup (server, after shutdown)', failures,
                        allow_unavailable_after_shutdown=args.allow_unavailable_stats_after_shutdown,
                    )
            server_stats['os_stats']['after_cleanup'] = process_snapshot()
        controller_log.event('closed', cleanup=cleanup)
        controller_log.close()

    native_hashes: set[str] = set()
    from ipc_paired import check_worker
    for worker in results:
        try:
            check_worker(worker, args.output_dir, False, source_commit(), args.calls, vars_for_config(args), failures)
        except (ValueError, OSError, KeyError, TypeError) as exc:
            failures.append(f"worker {worker.get('pid')} raw evidence: {exc}", getattr(exc, 'category', TOOL_FAILURE))
        if not worker.get('native_path') or not worker.get('native_sha256'):
            failures.tool(
                f"benchmark child {worker.get('pid')} reported no native path/hash; "
                'provenance must be present instead of an empty-set hash agreement'
            )
            continue
        native_hashes.add(worker['native_sha256'])
    if server_stats is not None:
        if not server_stats.get('native_path') or not server_stats.get('native_sha256'):
            failures.tool('controller server reported no native path/hash; provenance failure')
        else:
            native_hashes.add(server_stats['native_sha256'])
    if len(native_hashes) > 1:
        failures.tool(f'processes disagree on the imported native hash: {sorted(native_hashes)}')
    for record in child_processes:
        if record['exit_code'] is None:
            failures.tool(f"benchmark child {record['pid']} was never reaped (exit code is None)")
    recorded_pids = {record['pid'] for record in child_processes}
    if len(recorded_pids) != args.workers or len(child_processes) != args.workers:
        failures.tool(
            f'worker bookkeeping mismatch: --workers {args.workers}, spawned {len(children)}, '
            f'child records {len(child_processes)} ({len(recorded_pids)} distinct pids)'
        )
    report = {
        'schema': 'c-two.ipc-memory-benchmark.v2',
        'note': (
            'RSS is per-process peak, not summed shared backing or allocator usage; '
            'backing budget overrides are capacity configuration, not RSS measurements; '
            'no baseline improvement is inferred.'
        ),
        'argv': list(sys.argv),
        'python': args.python,
        'worker_command': command,
        'topology': args.topology,
        'source_commit': source_commit(), 'config': vars_for_config(args),
        'output_dir': str(args.output_dir), 'endpoint_root': endpoint_root,
        'barrier': barrier, 'events_path': str(controller_log.events_path),
        'internal_probes': INTERNAL_PROBES,
        'expected_workers': args.workers,
        'spawned_child_count': len(children),
        'stats_contract': {
            'require_memory_stats': args.require_memory_stats,
            'allow_unavailable_after_shutdown': args.allow_unavailable_stats_after_shutdown,
        },
        'server': server_stats,
        'server_memory_stats': server_memory_stats or None,
        'workers': results,
        'child_processes': child_processes,
        'children_terminated_pids': terminated,
        'controller_cleanup': cleanup,
        'failures': failures,
        'elapsed_ns': time.perf_counter_ns() - started,
    }
    report['ok'] = not failures
    return report


def matrix_rows() -> list[dict]:
    mib = 2**20
    return [
        {
            'name': 'idle_small_rpc_default_buddy', 'workload': 'idle_small',
            'config': 'default_buddy',
            'pool_enabled': True, 'segment_size': 2 * mib, 'payload_bytes': 1,
            'shm_threshold': 4096, 'calls': 200, 'workers': 1, 'topology': 'shared',
            'ipc_overrides': {}, 'expect_error_substring': None,
        },
        {
            'name': 'idle_small_rpc_buddy_disabled', 'workload': 'idle_small',
            'config': 'buddy_disabled',
            'pool_enabled': False, 'segment_size': 2 * mib, 'payload_bytes': 1,
            'shm_threshold': 4096, 'calls': 200, 'workers': 1, 'topology': 'shared',
            'ipc_overrides': {}, 'expect_error_substring': None,
        },
        {
            'name': 'burst4_shared_default_buddy', 'workload': 'burst_shared',
            'config': 'default_buddy',
            'pool_enabled': True, 'segment_size': 2 * mib, 'payload_bytes': 64 * 1024,
            'shm_threshold': 4096, 'calls': 40, 'workers': 4, 'topology': 'shared',
            'ipc_overrides': {}, 'expect_error_substring': None,
        },
        {
            'name': 'burst4_shared_buddy_disabled', 'workload': 'burst_shared',
            'config': 'buddy_disabled',
            'pool_enabled': False, 'segment_size': 2 * mib, 'payload_bytes': 64 * 1024,
            'shm_threshold': 4096, 'calls': 40, 'workers': 4, 'topology': 'shared',
            'ipc_overrides': {}, 'expect_error_substring': None,
        },
        {
            'name': 'science_payload_pairs_default_buddy', 'workload': 'science_payload',
            'config': 'default_buddy',
            'pool_enabled': True, 'segment_size': 2 * mib, 'payload_bytes': 4 * mib,
            'shm_threshold': 4096, 'calls': 8, 'workers': 1, 'topology': 'pairs',
            'ipc_overrides': {}, 'expect_error_substring': None,
        },
        {
            'name': 'science_payload_pairs_buddy_disabled', 'workload': 'science_payload',
            'config': 'buddy_disabled',
            'pool_enabled': False, 'segment_size': 2 * mib, 'payload_bytes': 4 * mib,
            'shm_threshold': 4096, 'calls': 8, 'workers': 1, 'topology': 'pairs',
            'ipc_overrides': {}, 'expect_error_substring': None,
        },
        {
            'name': 'forced_zero_shm_finite_file', 'workload': 'forced_budget',
            'config': 'zero_shm_finite_file',
            'pool_enabled': True, 'segment_size': mib, 'payload_bytes': 2 * mib,
            'shm_threshold': 64 * 1024, 'calls': 2, 'workers': 1, 'topology': 'shared',
            'ipc_overrides': {
                'shm_backing_budget_bytes': 0,
                'file_backing_budget_bytes': 64 * mib,
                'live_reassembly_budget_bytes': 64 * mib,
            },
            'expect_error_substring': None,
        },
        {
            'name': 'forced_zero_shm_finite_file_buddy_disabled', 'workload': 'forced_budget',
            'config': 'zero_shm_finite_file_buddy_disabled',
            'pool_enabled': False, 'segment_size': mib, 'payload_bytes': 2 * mib,
            'shm_threshold': 64 * 1024, 'calls': 2, 'workers': 1, 'topology': 'shared',
            'ipc_overrides': {
                'shm_backing_budget_bytes': 0,
                'file_backing_budget_bytes': 64 * mib,
                'live_reassembly_budget_bytes': 64 * mib,
            },
            'expect_error_substring': None,
        },
        {
            'name': 'forced_all_backing_zero_expect_capacity_error', 'workload': 'forced_budget',
            'config': 'all_backing_zero',
            'pool_enabled': True, 'segment_size': mib, 'payload_bytes': 2 * mib,
            'shm_threshold': 64 * 1024, 'calls': 1, 'workers': 1, 'topology': 'shared',
            'ipc_overrides': {
                'shm_backing_budget_bytes': 0,
                'file_backing_budget_bytes': 0,
                'live_reassembly_budget_bytes': 0,
            },
            'expect_error_substring': BUDGET_ERROR_PHRASE,
        },
    ]


def row_namespace(spec: dict) -> argparse.Namespace:
    return argparse.Namespace(
        pool_enabled=spec['pool_enabled'],
        segment_size=spec['segment_size'],
        payload_bytes=spec['payload_bytes'],
        shm_threshold=spec['shm_threshold'],
        calls=spec['calls'],
        workers=spec['workers'],
        topology=spec['topology'],
        ipc_overrides=spec['ipc_overrides'],
        expect_error_substring=spec['expect_error_substring'],
        memory_stats=bool(spec.get('memory_stats')),
        require_memory_stats=bool(spec.get('require_memory_stats')),
        allow_unavailable_stats_after_shutdown=bool(spec.get('allow_unavailable_stats_after_shutdown')),
        child_timeout=float(spec.get('child_timeout', 120.0)),
        python=spec['python'],
        address=None,
        worker=False,
        output=None,
        output_dir=Path(spec['output_dir']) if spec.get('output_dir') else None,
        warmup=spec.get('warmup', 3), call_timeout=spec.get('call_timeout', 30.0),
    )


def run_row(spec: dict) -> dict:
    benchmark = run_controller(row_namespace(spec))
    return {
        'schema': 'c-two.ipc-memory-row.v1',
        'row': {'name': spec['name'], 'workload': spec['workload'], 'config': spec['config']},
        'spec': spec,
        'benchmark': benchmark,
        'ok': bool(benchmark['ok']),
    }


def build_row_spec(table_row: dict, args: argparse.Namespace) -> dict:
    spec = dict(table_row)
    spec.update(
        memory_stats=args.memory_stats or table_row['workload'] == 'forced_budget',
        require_memory_stats=args.require_memory_stats or table_row['workload'] == 'forced_budget',
        allow_unavailable_stats_after_shutdown=args.allow_unavailable_stats_after_shutdown,
        child_timeout=args.child_timeout,
        python=args.python,
        warmup=getattr(args, 'warmup', 3), call_timeout=getattr(args, 'call_timeout', 30.0),
    )
    return spec


def validate_python_path(parser: argparse.ArgumentParser, python: str) -> None:
    if not Path(python).is_file():
        parser.error(f'--python {python!r} is not an existing file; pass an explicit interpreter path')


def validate_worker_count(parser: argparse.ArgumentParser, workers: object, source: str) -> None:
    if not isinstance(workers, int) or isinstance(workers, bool) or not 1 <= workers <= MAX_WORKERS:
        parser.error(f'{source} must be an integer in 1..={MAX_WORKERS}; got {workers!r}')


def validate_timeout(parser: argparse.ArgumentParser, value: object, name: str, maximum: float) -> None:
    if (
        not isinstance(value, (int, float))
        or isinstance(value, bool)
        or not math.isfinite(float(value))
        or not 0.0 < float(value) <= maximum
    ):
        parser.error(f'{name} must be a finite number in (0, {maximum}]; got {value!r}')


def validate_counts(
    parser: argparse.ArgumentParser,
    calls: object,
    workers: object,
    segment_size: object,
    shm_threshold: object,
    payload_bytes: object,
    source: str,
) -> None:
    label = f'{source} ' if source else ''
    validate_worker_count(parser, workers, f'{label}workers')
    for name, value in (
        ('calls', calls), ('segment_size', segment_size),
        ('shm_threshold', shm_threshold), ('payload_bytes', payload_bytes),
    ):
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            parser.error(f'{label}{name} must be a non-negative integer; got {value!r}')
    if min(calls, segment_size, shm_threshold) <= 0:
        parser.error(f'{label}calls/segment_size/shm_threshold must be positive')


def validate_row_spec(parser: argparse.ArgumentParser, spec: object) -> None:
    if not isinstance(spec, dict):
        parser.error('--row-spec must be a JSON object')
    required = {
        'name', 'workload', 'config', 'pool_enabled', 'segment_size', 'payload_bytes',
        'shm_threshold', 'calls', 'workers', 'topology', 'ipc_overrides', 'expect_error_substring',
        'python',
    }
    missing = sorted(required - set(spec))
    if missing:
        parser.error(f'--row-spec is missing required keys: {missing}')
    for key in ('pool_enabled', 'memory_stats', 'require_memory_stats', 'allow_unavailable_stats_after_shutdown'):
        if key in spec and type(spec[key]) is not bool:
            parser.error(f'--row-spec {key} must be a JSON boolean')
    expected = spec['expect_error_substring']
    if expected is not None and (not isinstance(expected, str) or not expected.strip()):
        parser.error('--row-spec expect_error_substring must be null or a non-empty string')
    if not isinstance(spec['python'], str) or not spec['python']:
        parser.error('--row-spec python must be a non-empty string')
    if spec['topology'] not in ('shared', 'pairs'):
        parser.error(f"--row-spec topology must be 'shared' or 'pairs'; got {spec['topology']!r}")
    if not isinstance(spec['ipc_overrides'], dict):
        parser.error('--row-spec ipc_overrides must be a JSON object')
    if not isinstance(spec['name'], str) or not spec['name']:
        parser.error('--row-spec name must be a non-empty string')
    validate_counts(
        parser, spec['calls'], spec['workers'], spec['segment_size'],
        spec['shm_threshold'], spec['payload_bytes'], '--row-spec',
    )
    validate_timeout(parser, spec.get('child_timeout', 120.0), '--row-spec child_timeout', MAX_CHILD_TIMEOUT_S)
    validate_timeout(parser, spec.get('call_timeout', 30.0), '--row-spec call_timeout', MAX_CHILD_TIMEOUT_S)
    if not isinstance(spec.get('warmup', 3), int) or isinstance(spec.get('warmup'), bool) or spec.get('warmup', 3) < 0:
        parser.error('--row-spec warmup must be a non-negative integer')
    validate_python_path(parser, spec['python'])


def terminate_owned_session(proc: subprocess.Popen, drain_timeout: float = POST_KILL_DRAIN_TIMEOUT_S) -> str | None:
    """Force-terminate a row controller and the session it leads, then drain its pipes.

    Only the session this matrix created is signalled: the child is started as
    its own session leader and the group is killed only after `os.getpgid`
    confirms the child still leads that group. The post-kill drain is bounded
    and always closes the pipes. Returns a drain-failure note when the pipes
    never close, and never treats the kill as clean shutdown.
    """
    if proc.poll() is None and hasattr(os, 'killpg') and hasattr(os, 'getpgid'):
        try:
            if os.getpgid(proc.pid) == proc.pid:
                os.killpg(proc.pid, signal.SIGKILL)
        except OSError:
            pass
    if proc.poll() is None:
        proc.kill()
    return drain_killed_child(proc, drain_timeout)


def row_job():
    return platform_process_support()._WindowsJob() if os.name == 'nt' else None


def run_matrix(args: argparse.Namespace) -> int:
    output_dir: Path = args.output_dir
    rows_dir = output_dir / 'rows'
    rows_dir.mkdir(parents=True, exist_ok=True)
    started_wall = time.time()
    started_ns = time.perf_counter_ns()

    all_rows = matrix_rows()
    if args.only:
        selectors = [part.strip() for part in args.only.split(',') if part.strip()]
        selected = [row for row in all_rows if any(selector in row['name'] for selector in selectors)]
        if not selected:
            raise SystemExit(f'--only {args.only!r} matched no matrix row')
    else:
        selected = all_rows

    matrix = {
        'schema': 'c-two.ipc-memory-matrix.v1',
        'note': (
            'Validation driver for the approved memory plan; raw latencies and RSS only, '
            'no throughput improvement claims. Forced-budget rows encode post-wiring '
            'expectations and are recorded honestly on candidates without Runtime budget '
            'wiring; their checks are never weakened. RSS stays separate from backing '
            'capacities; c_two.observe.memory_stats snapshot errors are recorded.'
        ),
        'generated_at_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(started_wall)),
        'source_commit': source_commit(),
        'matrix_argv': list(sys.argv),
        'python': args.python,
        'memory_stats': args.memory_stats,
        'require_memory_stats': args.require_memory_stats,
        'allow_unavailable_stats_after_shutdown': args.allow_unavailable_stats_after_shutdown,
        'child_timeout_s': args.child_timeout,
        'row_timeout_s': args.row_timeout,
        'selected_only': args.only,
        # complete is true only when every table row was executed. A successful
        # --only subset can exit 0 while complete stays false, so a partial run
        # is never mistaken for the full matrix.
        'complete': False,
        'rows': [],
        'failures': Failures(),
    }
    all_ok = True
    native_hashes: dict[str, str | None] = {}
    missing_provenance_rows: list[str] = []
    inconsistent_provenance_rows: list[str] = []
    skipped = [row['name'] for row in all_rows if row['name'] not in {r['name'] for r in selected}]
    for table_row in selected:
        spec = build_row_spec(table_row, args)
        spec['output_dir'] = str((output_dir / 'raw' / spec['name']).resolve())
        row_file = rows_dir / f"{spec['name']}.json"
        row_started = time.perf_counter_ns()
        terminated = False
        drain_failure = None
        row_record = None
        job = row_job()
        proc = subprocess.Popen(
            [args.python, str(Path(__file__).resolve()), '--python', args.python,
             '--row-spec', json.dumps(spec)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            start_new_session=job is None, creationflags=4 if job else 0,
        )
        try:
            if job is not None:
                job.assign_and_resume(proc)
            stdout, stderr = proc.communicate(timeout=args.row_timeout)
        except subprocess.TimeoutExpired:
            terminated = True
            drain_failure = terminate_owned_session(proc)
            stdout, stderr = '', ''
        except BaseException:
            terminate_owned_session(proc)
            raise
        else:
            if stdout.strip():
                try:
                    row_record = json.loads(stdout)
                except json.JSONDecodeError:
                    row_record = None
        finally:
            if job is not None:
                try:
                    if job.alive():
                        job.stop()
                        terminated = True
                        drain_failure = 'Windows row Job still had live descendants after controller completion'
                        deadline = time.perf_counter() + POST_KILL_DRAIN_TIMEOUT_S
                        while job.alive() and time.perf_counter() < deadline:
                            time.sleep(0.02)
                        if job.alive():
                            raise OSError('Windows row Job did not become empty after termination')
                finally:
                    job.close()
        elapsed_ns = time.perf_counter_ns() - row_started
        (rows_dir / f"{spec['name']}.stdout.json").write_text(stdout or '', encoding='utf-8')
        (rows_dir / f"{spec['name']}.stderr.log").write_text(stderr or '', encoding='utf-8')
        if terminated:
            row_record = {
                'schema': 'c-two.ipc-memory-row.v1',
                'row': {'name': spec['name'], 'workload': spec['workload'], 'config': spec['config']},
                'spec': spec,
                'ok': False,
                'failures': [failure_message(
                    f'row controller exceeded the {args.row_timeout}s join timeout; the row session '
                    'it led was terminated by the matrix. Forced termination is reported as a failure, '
                    'not clean shutdown.', TOOL_FAILURE)
                ] + ([failure_message(f'pipe drain: {drain_failure}', TOOL_FAILURE)] if drain_failure else []),
            }
        elif not isinstance(row_record, dict):
            row_record = {
                'schema': 'c-two.ipc-memory-row.v1',
                'row': {'name': spec['name'], 'workload': spec['workload'], 'config': spec['config']},
                'spec': spec,
                'ok': False,
                'failures': [failure_message(f'row controller exited with code {proc.returncode} without JSON output; '
                             f'stderr={(stderr or "").strip()[-2000:]}', TOOL_FAILURE)],
                'stderr_tail': (stderr or '').strip()[-2000:],
            }
        row_failures = row_record.get('failures', row_record.get('benchmark', {}).get('failures', []))
        if not isinstance(row_failures, list):
            row_failures = [failure_message('row controller emitted an invalid failures field', TOOL_FAILURE)]
        if proc.returncode != 0 and not terminated and not row_failures:
            row_failures.append(failure_message(f'row controller exited with nonzero status {proc.returncode} '
                                               'without classified failures', TOOL_FAILURE))
        row_record['failures'] = row_failures
        row_record['ok'] = row_record.get('ok') is True and proc.returncode == 0 and not terminated and not row_failures
        row_record['terminated_by_matrix'] = terminated
        row_record['elapsed_ns'] = elapsed_ns
        row_record['exit_code'] = 1 if terminated else proc.returncode
        row_file.write_text(json.dumps(row_record, indent=2) + '\n')

        benchmark = row_record.get('benchmark')
        benchmark = benchmark if isinstance(benchmark, dict) else {}
        worker_value = benchmark.get('workers')
        workers = worker_value if isinstance(worker_value, list) else []
        participants = list(workers)
        if spec['topology'] == 'shared':
            participants.append(benchmark.get('server'))
        missing_identity = len(workers) != spec['workers'] or any(
            not isinstance(item, dict) or not item.get('native_path') or not item.get('native_sha256')
            for item in participants
        )
        row_hashes = sorted({item['native_sha256'] for item in participants if isinstance(item, dict) and item.get('native_sha256')})
        if missing_identity or not row_hashes:
            native_hashes[spec['name']] = None
            missing_provenance_rows.append(spec['name'])
        elif len(row_hashes) == 1:
            native_hashes[spec['name']] = row_hashes[0]
        else:
            native_hashes[spec['name']] = 'inconsistent:' + ','.join(row_hashes)
            inconsistent_provenance_rows.append(spec['name'])
        matrix['rows'].append({
            'name': spec['name'],
            'workload': spec['workload'],
            'config': spec['config'],
            'ok': bool(row_record['ok']),
            'terminated_by_matrix': terminated,
            'exit_code': row_record['exit_code'],
            'elapsed_ns': elapsed_ns,
            'worker_count': spec['workers'],
            'native_sha256': native_hashes.get(spec['name']),
            'result_file': str(row_file.relative_to(output_dir)),
        })
        if not row_record['ok']:
            all_ok = False

    if missing_provenance_rows:
        matrix['failures'].tool(
            f'rows with missing process native path/hash: {sorted(missing_provenance_rows)}; '
            'missing provenance is a failure, not an empty-set agreement'
        )
    if inconsistent_provenance_rows:
        matrix['failures'].tool(
            f'rows whose processes disagree on the native hash: {sorted(inconsistent_provenance_rows)}'
        )
    distinct = sorted({value for value in native_hashes.values() if value and not value.startswith('inconsistent:')})
    matrix['native'] = {
        'per_row': native_hashes,
        'rows_missing_provenance': missing_provenance_rows,
        'rows_inconsistent': inconsistent_provenance_rows,
        'single_installed_hash': distinct[0] if len(distinct) == 1 and not missing_provenance_rows and not inconsistent_provenance_rows else None,
        'agreement': bool(native_hashes) and not missing_provenance_rows and not inconsistent_provenance_rows and len(distinct) <= 1,
    }
    matrix['rows_not_selected'] = skipped
    matrix['complete'] = not skipped
    matrix['totals'] = {
        'rows_total': len(all_rows),
        'rows_selected': len(matrix['rows']),
        'rows_not_selected': len(skipped),
        'passed': sum(1 for row in matrix['rows'] if row['ok']),
        'failed': sum(1 for row in matrix['rows'] if not row['ok']),
        'elapsed_ns': time.perf_counter_ns() - started_ns,
        'wall_seconds': round(time.time() - started_wall, 3),
    }
    if matrix['failures']:
        all_ok = False
    text = json.dumps(matrix, indent=2) + '\n'
    (output_dir / 'matrix.json').write_text(text)
    print(text, end='')
    return 0 if all_ok else 1


def list_rows() -> None:
    for row in matrix_rows():
        expectation = 'expect error: ' + row['expect_error_substring'] if row['expect_error_substring'] else 'expect success'
        print(
            f"{row['name']}\n    workload={row['workload']} config={row['config']} "
            f"payload={row['payload_bytes']}B calls={row['calls']} workers={row['workers']} "
            f"topology={row['topology']} pool_enabled={row['pool_enabled']} "
            f"ipc_overrides={json.dumps(row['ipc_overrides'], sort_keys=True)}\n    {expectation}"
        )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--mode', choices=['single', 'matrix', 'anomaly'], default='single')
    parser.add_argument('--pool-enabled', action=argparse.BooleanOptionalAction, default=True)
    parser.add_argument('--segment-size', type=int, default=2 * 1024 * 1024)
    parser.add_argument('--payload-bytes', type=int, default=1)
    parser.add_argument('--shm-threshold', type=int, default=4096)
    parser.add_argument('--calls', type=int, default=200)
    parser.add_argument('--warmup', type=int, default=3, help='Recorded warmup calls before the common start barrier')
    parser.add_argument('--call-timeout', type=float, default=30.0, help='Public per-call deadline; expired attempts stay in raw samples')
    parser.add_argument('--workers', type=int, default=1)
    parser.add_argument('--topology', choices=['shared', 'pairs'], default='shared')
    parser.add_argument('--ipc-overrides', type=json.loads, default={}, help='Additional native IPC overrides as a JSON object')
    parser.add_argument(
        '--expect-error-substring',
        help='Require the CRM call to raise an error containing this substring '
             f'(stable native phrase: {BUDGET_ERROR_PHRASE!r}); success in this mode fails validation',
    )
    parser.add_argument(
        '--memory-stats', action='store_true',
        help='Capture optional c_two.observe.memory_stats snapshots before/after calls and shutdown; '
             'record snapshot errors without fabricating counters',
    )
    parser.add_argument(
        '--require-memory-stats', action='store_true',
        help='Fail when any advertised c_two.observe.memory_stats snapshot point, including after cleanup, '
             'is unavailable instead of fabricating zeros',
    )
    parser.add_argument(
        '--allow-unavailable-stats-after-shutdown', action='store_true',
        help='Explicit after-cleanup stats contract: exempt only post-shutdown snapshots from '
             '--require-memory-stats, recording the exemption inside the snapshot',
    )
    parser.add_argument(
        '--child-timeout', type=float, default=120.0,
        help=f'Bounded join timeout per spawned child in seconds; finite and in (0, {MAX_CHILD_TIMEOUT_S}]',
    )
    parser.add_argument(
        '--row-timeout', type=float, default=300.0,
        help=f'Bounded join timeout per matrix row in seconds; finite and in (0, {MAX_ROW_TIMEOUT_S}]',
    )
    parser.add_argument('--python', default=sys.executable, help='Interpreter used for spawned benchmark processes')
    parser.add_argument('--output', type=Path)
    parser.add_argument('--output-dir', type=Path, help='Raw artifact directory; required for matrix and anomaly modes')
    parser.add_argument('--only', help='Comma-separated substrings; run matrix rows whose name contains any')
    parser.add_argument('--list-rows', action='store_true', help='Print the matrix row table and exit')
    parser.add_argument('--worker', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--address', help=argparse.SUPPRESS)
    parser.add_argument('--row-spec', help=argparse.SUPPRESS)
    parser.add_argument('--worker-index', type=int, default=0, help=argparse.SUPPRESS)
    parser.add_argument('--coord-dir', type=Path, help=argparse.SUPPRESS)
    parser.add_argument('--scenario', choices=[
        'hold', 'slow', 'stopped', 'fallback_dedicated', 'fallback_inline',
        'fallback_file', 'exhausted', 'release_idle', 'release_busy',
        'release_blocked', 'release_batch',
    ], default='hold')
    parser.add_argument('--control', choices=['alone', 'parallel'], default='parallel')
    parser.add_argument('--pause-seconds', type=float, default=0.25)
    parser.add_argument('--abnormal-calls', type=int, default=80)
    parser.add_argument('--anomaly-worker-spec', help=argparse.SUPPRESS)
    args = parser.parse_args()

    if args.list_rows:
        list_rows()
        return
    if not isinstance(args.ipc_overrides, dict):
        parser.error('--ipc-overrides must be a JSON object')
    if args.expect_error_substring is not None and not args.expect_error_substring.strip():
        parser.error('--expect-error-substring must be a non-empty substring')
    # Every bound is validated before any process is spawned, including for
    # --worker and --row-spec dispatch.
    validate_counts(
        parser, args.calls, args.workers, args.segment_size, args.shm_threshold, args.payload_bytes, '',
    )
    validate_timeout(parser, args.child_timeout, '--child-timeout', MAX_CHILD_TIMEOUT_S)
    validate_timeout(parser, args.row_timeout, '--row-timeout', MAX_ROW_TIMEOUT_S)
    validate_timeout(parser, args.call_timeout, '--call-timeout', MAX_CHILD_TIMEOUT_S)
    validate_timeout(parser, args.pause_seconds, '--pause-seconds', MAX_CHILD_TIMEOUT_S)
    if args.warmup < 0 or args.abnormal_calls < 1:
        parser.error('--warmup must be non-negative and --abnormal-calls positive')
    validate_python_path(parser, args.python)

    if args.row_spec:
        try:
            spec = json.loads(args.row_spec)
        except json.JSONDecodeError as exc:
            parser.error(f'--row-spec is not valid JSON: {exc}')
        validate_row_spec(parser, spec)
        row = run_row(spec)
        print(json.dumps(row, indent=2))
        sys.exit(0 if row['ok'] else 1)

    if args.anomaly_worker_spec:
        from ipc_anomaly import anomaly_worker
        report = anomaly_worker(json.loads(args.anomaly_worker_spec))
        print(json.dumps(report))
        sys.exit(0 if report['ok'] else 1)

    if args.worker:
        worker = run_worker(args)
        print(json.dumps(worker, indent=2))
        sys.exit(0 if worker['ok'] else 1)

    if args.mode == 'anomaly':
        if args.output_dir is None:
            parser.error('--mode anomaly requires --output-dir')
        from ipc_anomaly import run_anomaly
        report = run_anomaly(args)
        print(json.dumps(report, indent=2))
        sys.exit(0 if report['ok'] else 1)

    if args.mode == 'matrix':
        if args.output_dir is None:
            parser.error('--mode matrix requires --output-dir')
        sys.exit(run_matrix(args))

    report = run_controller(args)
    text = json.dumps(report, indent=2) + '\n'
    if args.output:
        args.output.write_text(text)
    print(text, end='')
    sys.exit(0 if report['ok'] else 1)


if __name__ == '__main__':
    main()
