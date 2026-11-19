"""Validate every anomaly configuration through the installed Runtime setters.

This check does not register a resource, connect a client, or allocate SHM.
Run in each built baseline/candidate environment before real anomaly runs.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
from types import SimpleNamespace

import ipc_memory as bench
from ipc_anomaly import scenario_spec
from ipc_paired import ANOMALIES


def check_configs(abnormal_calls: int) -> dict:
    os.environ['C2_ENV_FILE'] = ''
    os.environ['C2_RELAY_ANCHOR_ADDRESS'] = ''
    import c_two as cc
    from c_two import _native

    rows, failures = [], []
    for scenario in ANOMALIES:
        for control in ('alone', 'parallel'):
            config = scenario_spec(SimpleNamespace(
                scenario=scenario, control=control, abnormal_calls=abnormal_calls,
                calls=3, warmup=1, pause_seconds=0.2, call_timeout=30.0,
                child_timeout=120.0,
            ))
            row = {'scenario': scenario, 'control': control, 'accepted': {}}
            for role in ('server', 'normal', 'abnormal'):
                runtime = cc.Runtime()
                setter = runtime.set_server if role == 'server' else runtime.set_client
                options = config[role + '_options']
                try:
                    setter(**options)
                    row['accepted'][role] = options
                except (TypeError, ValueError, RuntimeError) as exc:
                    failures.append(f'{scenario}/{control}/{role}: {type(exc).__name__}: {exc}')
            route = cc.ConcurrencyConfig(**config['route_concurrency'])
            row['route_concurrency'] = {
                'mode': route.mode, 'max_workers': route.max_workers,
                'max_pending': route.max_pending,
            }
            if scenario == 'release_batch':
                inflight = config['inflight_calls']
                row['inflight_calls'] = inflight
                for role, key, expected in (
                    ('server', 'max_execution_workers', inflight + 1),
                    ('server', 'max_outstanding_calls', inflight + 1),
                    ('abnormal', 'max_outstanding_calls', inflight),
                ):
                    if role in row['accepted']:
                        actual = row['accepted'][role].get(key)
                        if actual != expected:
                            failures.append(f'{scenario}/{control}/{role}: {key}={actual}, expected {expected}')
            rows.append(row)
    return {
        'schema': 'c-two.ipc-anomaly-config-check.v1',
        'source_commit': bench.source_commit(),
        'native': bench.runtime_provenance(_native),
        'abnormal_calls': abnormal_calls, 'scenarios': len(ANOMALIES),
        'controls_checked': len(rows), 'native_updates_attempted': len(rows) * 3,
        'native_updates_passed': sum(len(row['accepted']) for row in rows),
        'rows': rows, 'failures': failures, 'ok': not failures,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--abnormal-calls', type=int, default=80)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    if args.abnormal_calls < 1:
        parser.error('--abnormal-calls must be positive')
    try:
        report = check_configs(args.abnormal_calls)
    except ImportError as exc:
        parser.exit(1, f'native config check requires the built C-Two environment: {exc}\n')
    text = json.dumps(report, indent=2) + '\n'
    if args.output:
        args.output.write_text(text, encoding='utf-8')
    print(text, end='')
    raise SystemExit(0 if report['ok'] else 1)


if __name__ == '__main__':
    main()
