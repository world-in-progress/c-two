"""Run the exact IPC matrix and anomaly controls in alternating source environments.

The candidate checkout supplies this harness to both interpreters. Every child
runs with its own source cwd and C2_BENCH_SOURCE_ROOT. Ratios are report-only;
correctness and missing evidence fail the run. Raw child artifacts are retained.
"""
from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import time

if str(Path(__file__).resolve().parent) not in sys.path:
    sys.path.insert(0, str(Path(__file__).resolve().parent))
from ipc_evidence import run_owned

BASELINE_HEAD = 'b861f994e9f7294c0f3d4efc096c14f0d7f5c121'
NORMAL_ROWS = (
    'idle_small_rpc_default_buddy', 'idle_small_rpc_buddy_disabled',
    'burst4_shared_default_buddy', 'burst4_shared_buddy_disabled',
    'science_payload_pairs_default_buddy', 'science_payload_pairs_buddy_disabled',
)
BUDGET_ROWS = (
    'forced_zero_shm_finite_file', 'forced_zero_shm_finite_file_buddy_disabled',
    'forced_all_backing_zero_expect_capacity_error',
)
ANOMALIES = (
    'hold', 'slow', 'stopped', 'fallback_dedicated', 'fallback_inline',
    'fallback_file', 'exhausted', 'release_idle', 'release_busy',
    'release_blocked', 'release_batch',
)


class EvidenceError(ValueError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise EvidenceError(message)


def git_head(root: Path) -> str:
    proc = subprocess.run(['git', 'rev-parse', 'HEAD'], cwd=root, text=True,
                          capture_output=True, check=True)
    return proc.stdout.strip()


def python_identity(python: Path, root: Path) -> dict:
    code = ('import importlib.metadata as m,json,platform,sys,sysconfig; '
            'deps={d.metadata["Name"].lower().replace("_","-"):d.version '
            'for d in m.distributions() if d.metadata.get("Name")}; '
            'deps.pop("c-two",None); '
            'print(json.dumps({"version":sys.version,"implementation":platform.python_implementation(),'
            '"architecture":platform.machine(),"platform":sys.platform,'
            '"sysconfig_platform":sysconfig.get_platform(),'
            '"dependencies":deps}))')
    proc = subprocess.run([str(python), '-c', code], cwd=root, text=True,
                          capture_output=True, check=True, timeout=60)
    return json.loads(proc.stdout)


def paired_order(index: int) -> tuple[str, str]:
    return ('baseline', 'candidate') if index % 2 == 0 else ('candidate', 'baseline')


def artifact_path(base: Path, value: object) -> Path:
    require(isinstance(value, str) and bool(value), 'missing artifact path')
    path = Path(value)
    return path if path.is_absolute() else base / path


def read_json(path: Path) -> dict:
    require(path.is_file(), f'missing report {path}')
    value = json.loads(path.read_text(encoding='utf-8'))
    require(isinstance(value, dict), f'non-object report {path}')
    return value


def sample_outcomes(path: Path, worker: dict, expected_config: dict | None) -> dict:
    require(path.is_file(), f'missing samples {path}')
    counts = {'attempts': 0, 'successes': 0, 'failures': 0, 'timeouts': 0}
    metadata = []
    measured = []
    seen_requests = set()
    identity = {key: worker.get(key) for key in ('role', 'pid', 'worker_index')}
    require(all(value is not None for value in identity.values()), 'missing worker identity')
    with path.open(encoding='utf-8') as stream:
        for line in stream:
            if line.strip():
                record = json.loads(line)
                require(isinstance(record, dict), f'bad sample in {path}')
                if record.get('kind') == 'run':
                    metadata.append(record)
                else:
                    require(record.get('kind') == 'sample', f'unknown sample record kind in {path}')
                    require(record.get('identity') == identity, f'sample identity mismatch in {path}')
                    request_id = record.get('request_id')
                    require(request_id is not None and request_id not in seen_requests,
                            f'duplicate or missing request ID in {path}')
                    seen_requests.add(request_id)
                    start, end, latency = (record.get(key) for key in
                                           ('start_ns', 'end_ns', 'latency_ns'))
                    require(all(type(value) is int and value >= 0 for value in (start, end, latency))
                            and end >= start and latency == end - start,
                            f'invalid sample timing in {path}')
                    require(record.get('status') in ('success', 'error', 'timeout'),
                            f'invalid sample status in {path}')
                    require(record.get('integrity_ok') is not False,
                            f'payload integrity failure in {path}')
                    if record['status'] == 'success':
                        require(record.get('integrity_ok') is True,
                                f'success without confirmed payload integrity in {path}')
                if record.get('kind') == 'sample' and record.get('measurement') is True:
                    counts['attempts'] += 1
                    measured.append(record)
                    status = record['status']
                    counts[{'success': 'successes', 'error': 'failures',
                            'timeout': 'timeouts'}[status]] += 1
    require(len(metadata) == 1 and metadata[0].get('identity') == identity,
            f'missing or duplicate run metadata/identity in {path}')
    require(metadata[0].get('provenance') == worker.get('provenance'),
            f'run metadata provenance mismatch in {path}')
    require(isinstance(metadata[0].get('config'), dict), f'missing run metadata config in {path}')
    if expected_config is not None:
        require(metadata[0]['config'] == expected_config, f'run metadata config mismatch in {path}')
    counts['latencies'] = sorted(record['latency_ns'] for record in measured)
    counts['elapsed_ns'] = (max(record['end_ns'] for record in measured)
                            - min(record['start_ns'] for record in measured)) if measured else None
    return counts


def check_worker(worker: dict, base: Path, normal: bool,
                 expected_commit: str | None = None, expected_attempts: int | None = None,
                 expected_config: dict | None = None) -> dict:
    require(isinstance(worker, dict), 'invalid worker record')
    summary = worker.get('summary')
    require(isinstance(summary, dict), 'missing worker summary')
    require(summary.get('quantile_method') == 'nearest-rank: sorted[ceil(p*n)-1]'
            and summary.get('quantile_population') ==
            'all measurement attempts, including errors and timeouts',
            'unexpected quantile definition')
    provenance = worker.get('provenance')
    require(isinstance(provenance, dict), 'missing worker provenance')
    require(provenance.get('module_path') and provenance.get('sha256'),
            'missing native module path/hash')
    require(provenance.get('module_path') == worker.get('native_path')
            and provenance.get('sha256') == worker.get('native_sha256'),
            'worker native identity disagrees with provenance')
    if expected_commit is not None:
        require(provenance.get('source_commit') == expected_commit,
                'worker source commit differs from checkout')
    samples = artifact_path(base, worker.get('samples_path'))
    events = artifact_path(base, worker.get('events_path'))
    require(events.is_file(), f'missing events {events}')
    attempts = summary.get('attempts')
    require(type(attempts) is int and attempts > 0, 'invalid attempts')
    outcomes = sample_outcomes(samples, worker, expected_config)
    require(outcomes['attempts'] == attempts, f'sample count differs from attempts: {samples}')
    if expected_attempts is not None:
        require(attempts == expected_attempts, f'incomplete attempt count: {samples}')
    for field in ('successes', 'failures', 'timeouts'):
        require(outcomes[field] == summary.get(field),
                f'summary {field} differs from raw samples: {samples}')
    require(summary.get('unsuccessful') == outcomes['failures'] + outcomes['timeouts'],
            f'summary unsuccessful differs from raw samples: {samples}')
    require(summary.get('failure_rate') == (outcomes['failures'] + outcomes['timeouts']) / attempts,
            f'summary failure rate differs from raw samples: {samples}')
    latencies = outcomes['latencies']
    for field, fraction in (('p50_ns', 0.5), ('p95_ns', 0.95), ('p99_ns', 0.99)):
        require(summary.get(field) == latencies[math.ceil(fraction * attempts) - 1],
                f'summary {field} differs from raw samples: {samples}')
    elapsed = outcomes['elapsed_ns']
    throughput = outcomes['successes'] * 1_000_000_000 / elapsed if elapsed else None
    require(summary.get('elapsed_ns') == elapsed
            and summary.get('throughput_calls_per_s') == throughput,
            f'summary throughput differs from raw samples: {samples}')
    if normal:
        require(summary.get('successes') == attempts and summary.get('failures') == 0
                and summary.get('timeouts') == 0 and summary.get('unsuccessful', 0) == 0,
                'normal row contains unsuccessful calls')
        for metric in ('p50_ns', 'p99_ns', 'throughput_calls_per_s'):
            value = summary.get(metric)
            require(type(value) in (int, float) and math.isfinite(value) and value > 0,
                    f'missing or invalid {metric}')
    return summary


def check_matrix(directory: Path, expected: set[str],
                 expected_commit: str | None = None) -> dict[str, list[dict]]:
    matrix = read_json(directory / 'matrix.json')
    require(not matrix.get('failures'), 'matrix reports failures')
    if expected_commit is not None:
        require(matrix.get('source_commit') == expected_commit,
                'matrix source commit differs from checkout')
    rows = matrix.get('rows')
    require(isinstance(rows, list), 'matrix has no rows')
    names = [row.get('name') for row in rows if isinstance(row, dict)]
    require(len(names) == len(rows) and len(names) == len(set(names)) and set(names) == expected,
            f'matrix rows mismatch: {names}')
    result = {}
    specs = {}
    hashes = set()
    for item in rows:
        name = item['name']
        require(item.get('ok') is True and item.get('exit_code') == 0, f'row failed: {name}')
        record = read_json(artifact_path(directory, item.get('result_file')))
        require(record.get('ok') is True and record.get('row', {}).get('name') == name,
                f'invalid row record: {name}')
        benchmark = record.get('benchmark')
        require(isinstance(benchmark, dict) and benchmark.get('ok') is True,
                f'benchmark failed: {name}')
        workers = benchmark.get('workers')
        spec = record.get('spec')
        require(isinstance(spec, dict), f'missing row config: {name}')
        specs[name] = {key: value for key, value in spec.items()
                       if key not in ('python', 'output_dir')}
        require(isinstance(workers, list) and len(workers) == spec.get('workers'),
                f'worker count mismatch: {name}')
        config_keys = ('pool_enabled', 'segment_size', 'payload_bytes', 'shm_threshold',
                       'calls', 'ipc_overrides', 'expect_error_substring', 'warmup',
                       'call_timeout')
        worker_config = {key: spec[key] for key in config_keys}
        result[name] = [check_worker(worker, directory, name in NORMAL_ROWS,
                                     expected_commit, spec['calls'], worker_config)
                        for worker in workers]
        hashes.add(item.get('native_sha256'))
    require(None not in hashes and len(hashes) == 1, 'matrix native hashes disagree or missing')
    require(matrix.get('native', {}).get('single_installed_hash') in hashes,
            'matrix native hash does not match row hash')
    result['__specs__'] = specs
    result['__native_sha256__'] = next(iter(hashes))
    return result


def ratio_report(groups: list[dict], rows: set[str]) -> dict:
    require(len(groups) >= 5, 'at least five complete paired groups are required')
    report = {}
    for name in sorted(rows & set(NORMAL_ROWS)):
        count = len(groups[0]['baseline'][name])
        require(count > 0, f'no workers: {name}')
        per_worker = []
        for worker_index in range(count):
            ratios = {'p50': [], 'p99': [], 'throughput': []}
            for group in groups:
                baseline = group['baseline'][name]
                candidate = group['candidate'][name]
                require(len(baseline) == len(candidate) == count, f'worker shape changed: {name}')
                b, c = baseline[worker_index], candidate[worker_index]
                require((b['quantile_method'], b['quantile_population']) ==
                        (c['quantile_method'], c['quantile_population']),
                        f'quantile definition changed: {name}')
                for key, field in (('p50', 'p50_ns'), ('p99', 'p99_ns'),
                                   ('throughput', 'throughput_calls_per_s')):
                    require(b[field] > 0 and c[field] > 0, f'invalid metric: {name}/{field}')
                    ratios[key].append(c[field] / b[field])
            medians = {key: statistics.median(values) for key, values in ratios.items()}
            per_worker.append({'index': worker_index, 'ratios': ratios, 'median_ratio': medians,
                               'thresholds_report_only': {'p50_at_most_1_05': medians['p50'] <= 1.05,
                               'p99_at_most_1_10': medians['p99'] <= 1.10,
                               'throughput_at_least_0_95': medians['throughput'] >= 0.95}})
        report[name] = per_worker
    return report


def run_harness(args, label: str, group: int, kind: str, scenario: str | None = None,
                control: str | None = None) -> Path:
    root = args.baseline_root if label == 'baseline' else args.candidate_root
    python = args.baseline_python if label == 'baseline' else args.candidate_python
    suffix = 'matrix' if kind == 'matrix' else f'{scenario}-{control}'
    output = args.output_dir / f'group-{group + 1:02d}' / label / suffix
    output.mkdir(parents=True, exist_ok=False)
    command = [str(python), str(args.candidate_root / 'tools/benchmarks/ipc_memory.py'),
               '--mode', kind, '--python', str(python), '--output-dir', str(output)]
    if kind == 'matrix':
        command += ['--memory-stats', '--require-memory-stats']
        if args.only:
            command += ['--only', ','.join(sorted(args.only))]
    else:
        command += ['--scenario', scenario, '--control', control,
                    '--calls', str(args.anomaly_calls), '--warmup', str(args.warmup),
                    '--pause-seconds', str(args.pause_seconds),
                    '--abnormal-calls', str(args.abnormal_calls)]
    env = dict(os.environ, C2_BENCH_SOURCE_ROOT=str(root), C2_RELAY_ANCHOR_ADDRESS='')
    with (output / 'process.log').open('w') as log:
        proc = run_owned(command, cwd=root, env=env, stdout=log,
                         timeout=args.timeout_seconds)
    require(proc.returncode == 0, f'{label} {suffix} exited {proc.returncode}; see {output / "process.log"}')
    return output


def check_anomaly(directory: Path, scenario: str, control: str,
                  expected_commit: str | None = None) -> dict:
    report = read_json(directory / 'anomaly.json')
    require(report.get('ok') is True and report.get('scenario') == scenario
            and report.get('control') == control, f'anomaly failed: {scenario}/{control}')
    require(not report.get('failures'), f'anomaly failures: {scenario}/{control}')
    if expected_commit is not None:
        require(report.get('source_commit') == expected_commit,
                'anomaly source commit differs from checkout')
    config = report.get('config')
    require(isinstance(config, dict), 'missing anomaly config')
    benchmark = report.get('benchmark')
    require(isinstance(benchmark, dict), 'missing anomaly benchmark')
    workers = benchmark.get('workers')
    require(isinstance(workers, list) and workers, 'missing anomaly workers')
    server = benchmark.get('server')
    require(isinstance(server, dict) and server.get('native_sha256'), 'missing anomaly server native hash')
    require(all(worker.get('native_sha256') == server['native_sha256'] for worker in workers),
            'anomaly process native hashes disagree')
    normal = [worker for worker in workers if worker.get('role') == 'normal'
              or worker.get('identity', {}).get('role') == 'normal']
    require(normal, 'missing normal anomaly worker')
    return {'config': config, 'native_sha256': server['native_sha256'],
            'normal_summaries': [check_worker(worker, directory, False, expected_commit,
                                               3 * config['calls'], config)
                                 for worker in normal]}


def run(args) -> dict:
    require(args.pairs >= 5, '--pairs must be at least 5')
    require(args.baseline_root.resolve() != args.candidate_root.resolve(), 'source roots must be separate')
    for root, python in ((args.baseline_root, args.baseline_python),
                         (args.candidate_root, args.candidate_python)):
        require(root.is_dir() and python.is_file(), f'missing source root or interpreter: {root}, {python}')
    baseline_head = git_head(args.baseline_root)
    candidate_head = git_head(args.candidate_root)
    require(baseline_head.startswith(args.baseline_commit), 'baseline checkout does not match pinned commit')
    baseline_fastdb = args.baseline_root.parent / 'fastdb'
    candidate_fastdb = args.candidate_root.parent / 'fastdb'
    require(baseline_fastdb.is_dir() == candidate_fastdb.is_dir(),
            'FastDB source checkout is present for only one environment')
    fastdb_head = None
    if baseline_fastdb.is_dir():
        fastdb_head = git_head(baseline_fastdb)
        require(fastdb_head == git_head(candidate_fastdb), 'FastDB source commits differ')
    baseline_identity = python_identity(args.baseline_python, args.baseline_root)
    candidate_identity = python_identity(args.candidate_python, args.candidate_root)
    require(baseline_identity == candidate_identity, 'Python platform or dependency mismatch between environments')
    selected = set(args.only) if args.only else set(NORMAL_ROWS + BUDGET_ROWS)
    require(selected <= set(NORMAL_ROWS + BUDGET_ROWS), 'unknown matrix row selector')
    require(not args.only or len(selected) == len(args.only), 'duplicate row selector')
    args.output_dir.mkdir(parents=True, exist_ok=False)
    receipt = {'schema': 'c-two.ipc-paired.v1', 'machine': platform.uname()._asdict(),
               'baseline_commit': baseline_head, 'candidate_commit': candidate_head,
               'baseline_ref_requested': args.baseline_ref,
               'candidate_ref_requested': args.candidate_ref,
               'fastdb_source_commit': fastdb_head,
               'baseline_root': str(args.baseline_root), 'candidate_root': str(args.candidate_root),
               'baseline_python': str(args.baseline_python), 'candidate_python': str(args.candidate_python),
               'python_identity': baseline_identity, 'selected_rows': sorted(selected),
               'pairs_requested': args.pairs, 'groups': [], 'anomalies': [], 'failures': []}
    for index in range(args.pairs):
        group = {'index': index + 1, 'order': list(paired_order(index))}
        anomaly_configs: dict[tuple[str, str], dict] = {}
        for label in paired_order(index):
            head = baseline_head if label == 'baseline' else candidate_head
            group[f'{label}_path'] = str(args.output_dir / f'group-{index + 1:02d}' / label / 'matrix')
            try:
                directory = run_harness(args, label, index, 'matrix')
                group[label] = check_matrix(directory, selected, head)
            except (EvidenceError, OSError, subprocess.SubprocessError, ValueError, KeyError) as exc:
                receipt['failures'].append(f'group {index + 1} {label} matrix: {exc}')
        if not args.skip_anomalies:
            for scenario in ANOMALIES:
                for control in ('alone', 'parallel'):
                    for label in paired_order(index):
                        head = baseline_head if label == 'baseline' else candidate_head
                        anomaly_path = args.output_dir / f'group-{index + 1:02d}' / label / f'{scenario}-{control}'
                        entry = {'group': index + 1, 'source': label, 'scenario': scenario,
                                 'control': control, 'path': str(anomaly_path)}
                        try:
                            anomaly_dir = run_harness(args, label, index, 'anomaly', scenario, control)
                            anomaly_result = check_anomaly(anomaly_dir, scenario, control, head)
                            config_key = (scenario, control)
                            if config_key in anomaly_configs:
                                require(anomaly_configs[config_key] == anomaly_result['config'],
                                        f'anomaly config mismatch: {scenario}/{control}')
                            else:
                                anomaly_configs[config_key] = anomaly_result['config']
                            entry.update({'native_sha256': anomaly_result['native_sha256'],
                                          'normal_summaries': anomaly_result['normal_summaries'],
                                          'ok': True})
                        except (EvidenceError, OSError, subprocess.SubprocessError, ValueError, KeyError) as exc:
                            entry.update({'ok': False, 'failure': str(exc)})
                            receipt['failures'].append(f'group {index + 1} {label} {scenario}/{control}: {exc}')
                        receipt['anomalies'].append(entry)
        if 'baseline' in group and 'candidate' in group:
            try:
                require(group['baseline'].get('__specs__') == group['candidate'].get('__specs__'),
                        f'baseline/candidate matrix config mismatch in group {index + 1}')
            except EvidenceError as exc:
                receipt['failures'].append(str(exc))
        receipt['groups'].append(group)
        (args.output_dir / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    if not receipt['failures']:
        try:
            receipt['ratios'] = ratio_report(receipt['groups'], selected)
        except EvidenceError as exc:
            receipt['failures'].append(str(exc))
    receipt['ok'] = not receipt['failures'] and len(receipt['groups']) >= 5
    (args.output_dir / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    return receipt


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('baseline-root', 'candidate-root', 'baseline-python', 'candidate-python', 'output-dir'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--baseline-commit', default=BASELINE_HEAD)
    parser.add_argument('--baseline-ref')
    parser.add_argument('--candidate-ref')
    parser.add_argument('--pairs', type=int, default=5)
    parser.add_argument('--only', help='Comma-separated exact matrix row names; development runs only')
    parser.add_argument('--skip-anomalies', action='store_true', help='Development runs only')
    parser.add_argument('--anomaly-calls', type=int, default=40)
    parser.add_argument('--abnormal-calls', type=int, default=40)
    parser.add_argument('--warmup', type=int, default=3)
    parser.add_argument('--pause-seconds', type=float, default=0.1)
    parser.add_argument('--timeout-seconds', type=float, default=3600)
    args = parser.parse_args()
    args.only = args.only.split(',') if args.only else None
    for name in ('pairs', 'anomaly_calls', 'abnormal_calls', 'warmup'):
        value = getattr(args, name)
        if value < (0 if name == 'warmup' else 1):
            parser.error(f'--{name.replace("_", "-")} must be positive')
    if not math.isfinite(args.pause_seconds) or args.pause_seconds <= 0:
        parser.error('--pause-seconds must be finite and positive')
    if not math.isfinite(args.timeout_seconds) or args.timeout_seconds <= 0:
        parser.error('--timeout-seconds must be finite and positive')
    result = run(args)
    print(json.dumps({'ok': result['ok'], 'failures': result['failures'],
                      'receipt': str(args.output_dir / 'receipt.json')}))
    raise SystemExit(0 if result['ok'] else 1)


if __name__ == '__main__':
    main()
