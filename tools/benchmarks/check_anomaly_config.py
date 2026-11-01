"""Resolve every anomaly configuration with the installed native parsers.

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
            row = {'scenario': scenario, 'control': control, 'resolved': {}}
            for role, resolver in (
                ('server', _native.resolve_server_ipc_config),
                ('normal', _native.resolve_client_ipc_config),
                ('abnormal', _native.resolve_client_ipc_config),
            ):
                try:
                    row['resolved'][role] = dict(resolver(
                        overrides=config[role + '_overrides'],
                        global_overrides={'shm_threshold': config['shm_threshold']},
                    ))
                except Exception as exc:
                    failures.append(f'{scenario}/{control}/{role}: {type(exc).__name__}: {exc}')
            route = cc.ConcurrencyConfig(**config['route_concurrency'])
            row['route_concurrency'] = {
                'mode': route.mode, 'max_workers': route.max_workers,
                'max_pending': route.max_pending,
            }
            if scenario == 'release_batch' and 'server' in row['resolved']:
                resolved_workers = row['resolved']['server']['max_execution_workers']
                row['inflight_calls'] = config['inflight_calls']
                if resolved_workers < config['inflight_calls'] + 1:
                    failures.append(f'{scenario}/{control}: insufficient resolved execution workers: {resolved_workers}')
            rows.append(row)
    return {
        'schema': 'c-two.ipc-anomaly-config-check.v1',
        'source_commit': bench.source_commit(),
        'native': bench.runtime_provenance(_native),
        'abnormal_calls': abnormal_calls, 'scenarios': len(ANOMALIES),
        'controls_checked': len(rows), 'native_resolutions_attempted': len(rows) * 3,
        'native_resolutions_passed': sum(len(row['resolved']) for row in rows),
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
