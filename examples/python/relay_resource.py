"""Relay-backed Grid resource process.

Run after starting ``c3 relay``:
    uv run python examples/python/relay_resource.py --relay-url http://127.0.0.1:8300
"""
from __future__ import annotations

import argparse
import logging
import sys
from pathlib import Path

EXAMPLES_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(EXAMPLES_ROOT))

import c_two as cc
from grid.nested_grid import NestedGrid
from grid.grid_py_crm import GridPython
from relay_config import ensure_http_relay_url, resolved_relay_url

DEFAULT_RELAY_URL = 'http://127.0.0.1:8300'

logging.basicConfig(level=logging.INFO)


def _relay_url(value: str) -> str:
    try:
        return ensure_http_relay_url(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(str(exc)) from exc


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description='Start the Grid CRM and register it with an HTTP relay.',
    )
    parser.add_argument(
        '--relay-url',
        type=_relay_url,
        default=resolved_relay_url(DEFAULT_RELAY_URL),
        help=(
            'HTTP relay URL used for name registration '
            f'(default: C2_RELAY_ANCHOR_ADDRESS or {DEFAULT_RELAY_URL}).'
        ),
    )
    args = parser.parse_args(argv)
    try:
        args.relay_url = ensure_http_relay_url(args.relay_url)
    except ValueError as exc:
        parser.error(str(exc))
    return args


def main() -> None:
    args = parse_args()
    cc.set_relay_anchor(args.relay_url)

    grid = NestedGrid(
        2326,
        [808357.5, 824117.5, 838949.5, 843957.5],
        [64.0, 64.0],
        [[478, 310], [2, 2], [2, 2], [2, 2], [2, 2], [2, 2], [1, 1]],
    )

    cc.register(GridPython, grid, name='examples/grid')
    print(f'Grid CRM registered at {cc.server_address()}', flush=True)
    cc.serve()


if __name__ == '__main__':
    main()
