"""Relay-backed FastDB Grid resource process.

Run after starting ``c3 relay``:
    uv run python examples/python/fastdb_relay_resource.py --relay-url http://127.0.0.1:8300
"""
from __future__ import annotations

import argparse
import logging
import sys
from pathlib import Path

EXAMPLES_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(EXAMPLES_ROOT))

import c_two as cc
from grid.fastdb_bridge import grid_fastdb_bridge
from grid.grid_fdb_crm import GridFastdb
from grid.nested_grid import NestedGrid
from relay_config import ensure_http_relay_url, resolved_relay_url

DEFAULT_RELAY_URL = 'http://127.0.0.1:8300'
ROUTE_NAME = 'examples/grid-fastdb'

logging.basicConfig(level=logging.INFO)


def _relay_url(value: str) -> str:
    try:
        return ensure_http_relay_url(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(str(exc)) from exc


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description='Start the FastDB-backed Grid CRM and register it with an HTTP relay.',
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
        epsg=4326,
        bounds=[0.0, 0.0, 4.0, 4.0],
        first_size=[2.0, 2.0],
        subdivide_rules=[[2, 2], [2, 2], [2, 2]],
    )

    cc.register(
        GridFastdb,
        grid,
        name=ROUTE_NAME,
        bridge=grid_fastdb_bridge(),
    )
    print(f'FastDB Grid CRM registered at {cc.server_address()}', flush=True)
    cc.serve()


if __name__ == '__main__':
    main()
