from __future__ import annotations

import argparse
import importlib
import json
import sys
from pathlib import Path
from typing import Sequence

from c_two.contract import (
    diagnostics,
    export_descriptor,
)


def main(argv: Sequence[str] | None = None) -> int:
    parser = _parser()
    args = parser.parse_args(argv)
    try:
        if args.command == 'diagnose':
            contract = _load_contract(args.target)
            payload = json_payload(
                diagnostics(
                    contract,
                    methods=args.method or None,
                ),
                pretty=args.pretty,
            )
            _write_payload(payload, args.out)
            return 0
        if args.command == 'export':
            contract = _load_contract(args.target)
            payload = export_descriptor(
                contract,
                methods=args.method or None,
                pretty=args.pretty,
            )
            _write_payload(payload, args.out)
            return 0
    except Exception as exc:
        print(f'c-two contract {args.command} failed: {exc}', file=sys.stderr)
        return 2
    parser.error(f'unsupported command {args.command!r}')
    return 2


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog='python -m c_two.cli.contract')
    subparsers = parser.add_subparsers(dest='command', required=True)
    diagnose = subparsers.add_parser('diagnose')
    diagnose.add_argument('target', help='Python CRM class target as module:ClassName')
    diagnose.add_argument('--method', action='append', default=[], help='Limit diagnostics to one CRM method; repeatable')
    diagnose.add_argument('--out', help='Write diagnostics JSON to this file instead of stdout')
    diagnose.add_argument('--pretty', action='store_true', help='Pretty-print diagnostics JSON')
    export = subparsers.add_parser('export')
    export.add_argument('target', help='Python CRM class target as module:ClassName')
    export.add_argument('--method', action='append', default=[], help='Limit export to one CRM method; repeatable')
    export.add_argument('--out', help='Write descriptor JSON to this file instead of stdout')
    export.add_argument('--pretty', action='store_true', help='Pretty-print descriptor JSON')
    return parser


def _load_contract(target: str) -> type:
    module_name, sep, attr_name = target.partition(':')
    if not sep or not module_name or not attr_name:
        raise ValueError('target must use module:ClassName syntax.')
    module = importlib.import_module(module_name)
    value = module
    for part in attr_name.split('.'):
        value = getattr(value, part)
    if not isinstance(value, type):
        raise TypeError(f'{target} did not resolve to a class.')
    return value


def _write_payload(payload: str, out: str | None) -> None:
    if out:
        Path(out).write_text(payload, encoding='utf-8')
    else:
        if not payload.endswith('\n'):
            payload += '\n'
        sys.stdout.flush()
        sys.stdout.buffer.write(payload.encode('utf-8'))
        sys.stdout.buffer.flush()


def json_payload(value: object, *, pretty: bool) -> str:
    if pretty:
        return json.dumps(value, sort_keys=True, indent=2) + '\n'
    return json.dumps(value, sort_keys=True, separators=(',', ':'))


if __name__ == '__main__':
    raise SystemExit(main())
