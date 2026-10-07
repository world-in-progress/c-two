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

RSS is a process high-water mark, including Python and serialization; it is
not an allocator byte counter and never a backing-capacity measurement, so
the report keeps OS RSS, configured backing budgets, and held-buffer counts
in separate fields. Windows reports null when resource is absent. Runtime
memory snapshots use the public cc.memory_stats when present; when the API
is absent the snapshot is recorded as unavailable, and --require-memory-stats
fails instead of fabricating zero values. In require mode every advertised
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
import time
import traceback


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
    state = repo_state(Path(__file__))
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


def snapshot_memory_stats(cc) -> dict:
    """One runtime snapshot through the public API, or an explicit unavailable record."""
    if not hasattr(cc, 'memory_stats'):
        return {'available': False, 'reason': 'installed c_two exposes no public cc.memory_stats'}
    try:
        return {'available': True, 'stats': dict(cc.memory_stats())}
    except Exception as exc:  # recorded honestly, never replaced with zeros
        return {'available': True, 'error': f'{type(exc).__name__}: {exc}'}


def memory_stats_usable(snapshot: dict) -> bool:
    return bool(snapshot.get('available')) and 'stats' in snapshot and 'error' not in snapshot


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
    failures.append(f'memory stats required but unavailable at {point}')


def measure(args: argparse.Namespace) -> dict:
    os.environ['C2_ENV_FILE'] = ''
    os.environ['C2_RELAY_ANCHOR_ADDRESS'] = ''
    import c_two as cc
    from c_two import _native

    failures: list[str] = []
    cleanup: dict[str, object] = {}
    Echo, EchoResource = echo_types(cc)
    overrides = configure(cc, args)
    rss = {'imported': peak_rss_bytes()}
    stats_enabled = args.memory_stats or args.require_memory_stats
    memory_stats: dict[str, dict] = {}
    if stats_enabled:
        memory_stats['before_connect'] = snapshot_memory_stats(cc)
        if args.require_memory_stats:
            require_snapshot(memory_stats['before_connect'], 'before_connect (worker)', failures)

    expected = args.expect_error_substring
    expected_error = None
    if expected is not None:
        expected_error = {
            'required_substring': expected,
            'required_stage': 'call',
            'raised': False,
            'stage': None,
            'stage_matched': None,
            'error_type': None,
            'error_message': None,
            'observed': None,
            'control_ping_ok': None,
            'matched': None,
        }

    payload = payload_pattern('c-two-ipc-memory', args.payload_bytes)
    client = None
    address = args.address
    latencies: list[int] = []
    connected_ns = None
    held_stats = None
    after_release = None
    started = time.perf_counter_ns()
    try:
        try:
            if address is None:
                cc.register(Echo, EchoResource(), name='echo')
                rss['registered'] = peak_rss_bytes()
                address = cc.server_address()
            try:
                client = cc.connect(Echo, name='echo', address=address)
            except Exception as exc:
                if expected is None:
                    raise
                expected_error.update(
                    raised=True, stage='connect',
                    error_type=type(exc).__name__, error_message=str(exc),
                )
            else:
                connected_ns = time.perf_counter_ns() - started
                rss['connected'] = peak_rss_bytes()
                for _ in range(args.calls):
                    before = time.perf_counter_ns()
                    try:
                        result = client.echo(payload)
                    except Exception as exc:
                        if expected is None:
                            raise
                        expected_error.update(
                            raised=True, stage='call',
                            error_type=type(exc).__name__, error_message=str(exc),
                        )
                        break
                    elapsed = time.perf_counter_ns() - before
                    if result != payload:
                        raise RuntimeError('IPC payload mismatch')
                    latencies.append(elapsed)
                if expected is not None and not expected_error['raised']:
                    failures.append('expected native capacity error was not raised; the call succeeded')
            if expected_error is not None and expected_error['raised']:
                observed = f"{expected_error['error_type']}: {expected_error['error_message']}"
                expected_error['observed'] = observed
                expected_error['matched'] = expected in observed
                expected_error['stage_matched'] = expected_error['stage'] == 'call'
                if not expected_error['matched']:
                    failures.append(f'raised error lacks required substring {expected!r}: {observed}')
                if not expected_error['stage_matched']:
                    failures.append(
                        'expected the native capacity error at the call stage, but it was raised '
                        f"during {expected_error['stage']}: {observed}; a connect-stage error is not "
                        'the failure under test even when the substring and control ping match'
                    )
                from c_two.transport.client import util
                try:
                    ping_ok = bool(util.ping(address, timeout=2.0))
                except Exception as exc:
                    expected_error['control_ping_ok'] = False
                    failures.append(f'direct IPC control ping raised {type(exc).__name__}: {exc}')
                else:
                    expected_error['control_ping_ok'] = ping_ok
                    if not ping_ok:
                        failures.append('direct IPC control ping failed after the capacity error')
            if expected_error is None and client is not None:
                held = cc.hold(client.echo)(payload)
                try:
                    if held.value != payload:
                        raise RuntimeError('held IPC payload mismatch')
                    held_stats = cc.hold_stats()
                finally:
                    held.release()
                after_release = cc.hold_stats()
                if after_release['active_holds'] != 0:
                    raise RuntimeError('benchmark retained a transport hold')
        except Exception as exc:
            # Recorded as a failure so the artifact still carries cleanup,
            # latency, and RSS evidence for the failing run.
            failures.append(f'{type(exc).__name__}: {exc}')
        if stats_enabled:
            memory_stats['after_calls'] = snapshot_memory_stats(cc)
            if args.require_memory_stats:
                require_snapshot(memory_stats['after_calls'], 'after_calls (worker)', failures)
    finally:
        if client is not None:
            try:
                cc.close(client)
                cleanup['close_client'] = 'ok'
            except Exception as exc:
                cleanup['close_client'] = f'{type(exc).__name__}: {exc}'
                failures.append('client close failed after the run')
        try:
            cc.shutdown()
            cleanup['shutdown'] = 'ok'
        except Exception as exc:
            cleanup['shutdown'] = f'{type(exc).__name__}: {exc}'
            failures.append('cc.shutdown failed after the run')
        if stats_enabled:
            memory_stats['after_cleanup'] = snapshot_memory_stats(cc)
            if args.require_memory_stats:
                require_snapshot(
                    memory_stats['after_cleanup'], 'after_cleanup (worker, after shutdown)', failures,
                    allow_unavailable_after_shutdown=args.allow_unavailable_stats_after_shutdown,
                )

    ordered = sorted(latencies)
    rpc_elapsed_ns = sum(latencies) if latencies else None
    provenance = runtime_provenance(_native)
    if not provenance['module_path'] or not provenance['sha256']:
        failures.append(
            'native provenance missing in the worker: '
            f"module_path={provenance['module_path']!r} sha256={provenance['sha256']!r}"
        )
    result = {
        'pid': os.getpid(),
        'python_executable': sys.executable,
        'version': distribution_version('c-two'),
        'fastdb_version': distribution_version('fastdb4py'),
        'native_path': provenance['module_path'],
        'native_sha256': provenance['sha256'],
        'native_repo': provenance['repo'],
        'python': sys.version,
        'platform': sys.platform,
        'argv': list(sys.argv),
        'overrides': overrides,
        'stats_contract': {
            'require_memory_stats': args.require_memory_stats,
            'allow_unavailable_after_shutdown': args.allow_unavailable_stats_after_shutdown,
        },
        'shm_threshold': args.shm_threshold,
        'payload_bytes': args.payload_bytes,
        'calls': args.calls,
        'calls_completed': len(latencies),
        'connect_ns': connected_ns,
        'first_call_ns': latencies[0] if latencies else None,
        'median_call_ns': statistics.median(latencies) if latencies else None,
        'p95_call_ns': ordered[min(len(ordered) - 1, (95 * len(ordered) + 99) // 100 - 1)] if ordered else None,
        'rpc_elapsed_ns': rpc_elapsed_ns,
        # Throughput is derived from measured call latencies only: no connect,
        # import, or payload-build time is folded into it.
        'throughput_calls_per_s': len(latencies) / (rpc_elapsed_ns / 1e9) if rpc_elapsed_ns else None,
        'rss_high_water_bytes': rss,
        'held': held_stats,
        'after_release': after_release,
        'memory_stats': memory_stats or None,
        'expected_error': expected_error,
        'cleanup': cleanup,
        'failures': failures,
    }
    result['ok'] = not failures
    return result


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
            'failures': [f'{type(exc).__name__}: {exc}'],
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
    command = [
        args.python, str(Path(__file__).resolve()), '--worker',
        '--pool-enabled' if args.pool_enabled else '--no-pool-enabled',
        '--segment-size', str(args.segment_size),
        '--payload-bytes', str(args.payload_bytes),
        '--shm-threshold', str(args.shm_threshold), '--calls', str(args.calls),
        '--ipc-overrides', json.dumps(args.ipc_overrides),
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
    failures: list[str] = []
    cleanup: dict[str, object] = {}
    server_cc = None
    server_stats = None
    server_memory_stats: dict[str, dict] = {}
    stats_enabled = args.memory_stats or args.require_memory_stats
    started = time.perf_counter_ns()
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
            }
            if stats_enabled:
                server_memory_stats['after_register'] = snapshot_memory_stats(server_cc)
                if args.require_memory_stats:
                    require_snapshot(server_memory_stats['after_register'], 'after_register (server)', failures)
        for _ in range(args.workers):
            children.append(subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True))
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
                failures.append(
                    f'benchmark child {child.pid} exceeded the {args.child_timeout}s join timeout '
                    'and was terminated; termination is reported as a failure'
                )
                if drain_note:
                    failures.append(f'benchmark child {child.pid} post-kill drain failed: {drain_note}')
                continue
            child_processes.append({'pid': child.pid, 'exit_code': child.returncode, 'forced_termination': False})
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
                failures.append(
                    f'benchmark child {child.pid} failed with exit code {child.returncode}: {detail}'
                )
                if isinstance(worker_result, dict):
                    results.append(worker_result)
                continue
            if not isinstance(worker_result, dict):
                failures.append(f'benchmark child {child.pid} produced no JSON result')
                continue
            if not worker_result.get('ok'):
                failures.append(f'benchmark child {child.pid} reported validation failures')
            results.append(worker_result)
        if server_stats is not None:
            server_stats['rss_after_calls_high_water_bytes'] = peak_rss_bytes()
            if stats_enabled:
                server_memory_stats['after_workers'] = snapshot_memory_stats(server_cc)
                if args.require_memory_stats:
                    require_snapshot(server_memory_stats['after_workers'], 'after_workers (server)', failures)
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
                    failures.append(
                        f'benchmark child {child.pid} was still running during cleanup and was terminated'
                    )
                if drain_note:
                    failures.append(f'benchmark child {child.pid} post-kill drain failed: {drain_note}')
        if server_cc is not None:
            try:
                server_cc.shutdown()
                cleanup['server_shutdown'] = 'ok'
            except Exception as exc:
                cleanup['server_shutdown'] = f'{type(exc).__name__}: {exc}'
                failures.append('controller server shutdown failed after the run')
            if stats_enabled:
                server_memory_stats['after_cleanup'] = snapshot_memory_stats(server_cc)
                if args.require_memory_stats:
                    require_snapshot(
                        server_memory_stats['after_cleanup'], 'after_cleanup (server, after shutdown)', failures,
                        allow_unavailable_after_shutdown=args.allow_unavailable_stats_after_shutdown,
                    )

    native_hashes: set[str] = set()
    for worker in results:
        if not worker.get('native_path') or not worker.get('native_sha256'):
            failures.append(
                f"benchmark child {worker.get('pid')} reported no native path/hash; "
                'provenance must be present instead of an empty-set hash agreement'
            )
            continue
        native_hashes.add(worker['native_sha256'])
    if server_stats is not None:
        if not server_stats.get('native_path') or not server_stats.get('native_sha256'):
            failures.append('controller server reported no native path/hash; provenance failure')
        else:
            native_hashes.add(server_stats['native_sha256'])
    if len(native_hashes) > 1:
        failures.append(f'processes disagree on the imported native hash: {sorted(native_hashes)}')
    for record in child_processes:
        if record['exit_code'] is None:
            failures.append(f"benchmark child {record['pid']} was never reaped (exit code is None)")
    recorded_pids = {record['pid'] for record in child_processes}
    if len(recorded_pids) != args.workers or len(child_processes) != args.workers:
        failures.append(
            f'worker bookkeeping mismatch: --workers {args.workers}, spawned {len(children)}, '
            f'child records {len(child_processes)} ({len(recorded_pids)} distinct pids)'
        )
    report = {
        'schema': 'c-two.ipc-memory-benchmark.v1',
        'note': (
            'RSS is per-process peak, not summed shared backing or allocator usage; '
            'backing budget overrides are capacity configuration, not RSS measurements; '
            'no baseline improvement is inferred.'
        ),
        'argv': list(sys.argv),
        'python': args.python,
        'worker_command': command,
        'topology': args.topology,
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
        memory_stats=args.memory_stats,
        require_memory_stats=args.require_memory_stats,
        allow_unavailable_stats_after_shutdown=args.allow_unavailable_stats_after_shutdown,
        child_timeout=args.child_timeout,
        python=args.python,
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
            'capacities; cc.memory_stats snapshots are recorded as unavailable when absent.'
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
        'failures': [],
    }
    all_ok = True
    native_hashes: dict[str, str | None] = {}
    missing_provenance_rows: list[str] = []
    inconsistent_provenance_rows: list[str] = []
    skipped = [row['name'] for row in all_rows if row['name'] not in {r['name'] for r in selected}]
    for table_row in selected:
        spec = build_row_spec(table_row, args)
        row_file = rows_dir / f"{spec['name']}.json"
        row_started = time.perf_counter_ns()
        terminated = False
        drain_failure = None
        row_record = None
        proc = subprocess.Popen(
            [args.python, str(Path(__file__).resolve()), '--python', args.python,
             '--row-spec', json.dumps(spec)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True,
        )
        try:
            stdout, stderr = proc.communicate(timeout=args.row_timeout)
        except subprocess.TimeoutExpired:
            terminated = True
            drain_failure = terminate_owned_session(proc)
            stdout, stderr = '', ''
        else:
            if stdout.strip():
                try:
                    row_record = json.loads(stdout)
                except json.JSONDecodeError:
                    row_record = None
        elapsed_ns = time.perf_counter_ns() - row_started
        if terminated:
            row_record = {
                'schema': 'c-two.ipc-memory-row.v1',
                'row': {'name': spec['name'], 'workload': spec['workload'], 'config': spec['config']},
                'spec': spec,
                'ok': False,
                'failures': [
                    f'row controller exceeded the {args.row_timeout}s join timeout; the row session '
                    'it led was terminated by the matrix. Forced termination is reported as a failure, '
                    'not clean shutdown.'
                ] + ([f'pipe drain: {drain_failure}'] if drain_failure else []),
            }
        elif not isinstance(row_record, dict):
            row_record = {
                'schema': 'c-two.ipc-memory-row.v1',
                'row': {'name': spec['name'], 'workload': spec['workload'], 'config': spec['config']},
                'spec': spec,
                'ok': False,
                'failures': [f'row controller exited with code {proc.returncode} without JSON output'],
                'stderr_tail': (stderr or '').strip()[-2000:],
            }
        row_failures = row_record.get('failures', [])
        if not isinstance(row_failures, list):
            row_failures = ['row controller emitted an invalid failures field']
        if proc.returncode != 0 and not terminated:
            row_failures.append(f'row controller exited with nonzero status {proc.returncode}')
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
        matrix['failures'].append(
            f'rows with missing process native path/hash: {sorted(missing_provenance_rows)}; '
            'missing provenance is a failure, not an empty-set agreement'
        )
    if inconsistent_provenance_rows:
        matrix['failures'].append(
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
    parser.add_argument('--mode', choices=['single', 'matrix'], default='single')
    parser.add_argument('--pool-enabled', action=argparse.BooleanOptionalAction, default=True)
    parser.add_argument('--segment-size', type=int, default=2 * 1024 * 1024)
    parser.add_argument('--payload-bytes', type=int, default=1)
    parser.add_argument('--shm-threshold', type=int, default=4096)
    parser.add_argument('--calls', type=int, default=200)
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
        help='Capture optional cc.memory_stats snapshots before/after calls and shutdown; '
             'recorded as unavailable when the public API is absent',
    )
    parser.add_argument(
        '--require-memory-stats', action='store_true',
        help='Fail when any advertised cc.memory_stats snapshot point, including after cleanup, '
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
    parser.add_argument('--output-dir', type=Path, help='Artifact directory for --mode matrix')
    parser.add_argument('--only', help='Comma-separated substrings; run matrix rows whose name contains any')
    parser.add_argument('--list-rows', action='store_true', help='Print the matrix row table and exit')
    parser.add_argument('--worker', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--address', help=argparse.SUPPRESS)
    parser.add_argument('--row-spec', help=argparse.SUPPRESS)
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

    if args.worker:
        worker = run_worker(args)
        print(json.dumps(worker, indent=2))
        sys.exit(0 if worker['ok'] else 1)

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
