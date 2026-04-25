"""SOTA API — client process.

Connects to the Grid CRM process. With ``--relay-url`` or ``C2_RELAY_ADDRESS``,
the client resolves the CRM by name through the relay. Without a relay, pass the
printed IPC address from ``crm_process.py``.

IPC:
    uv run python examples/python/crm_process.py
    uv run python examples/python/client.py <address>

Relay:
    uv run python examples/python/crm_process.py --relay-url http://127.0.0.1:8300
    uv run python examples/python/client.py --relay-url http://127.0.0.1:8300
"""
import argparse
import os
import sys
from pathlib import Path

EXAMPLES_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(EXAMPLES_ROOT))

import c_two as cc
from grid.grid_contract import (
    Grid,
    GridAttribute,
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description='Connect to the Grid CRM process via relay name resolution or IPC address.',
    )
    parser.add_argument(
        'address',
        nargs='?',
        help='Server IPC address printed by examples/python/crm_process.py, such as ipc://...',
    )
    parser.add_argument(
        '--relay-url',
        default=os.environ.get('C2_RELAY_ADDRESS'),
        help=(
            'HTTP relay URL used for name resolution. When set, the IPC address '
            f'is optional. Example: {'http://127.0.0.1:8300'}.'
        ),
    )
    args = parser.parse_args()
    if not args.relay_url and not args.address:
        parser.error('address is required when --relay-url or C2_RELAY_ADDRESS is not set')
    return args


def main():
    args = parse_args()

    if args.relay_url:
        cc.set_relay(args.relay_url)
        grid = cc.connect(Grid, name='examples/grid')
    else:
        grid = cc.connect(Grid, name='examples/grid', address=args.address)
    print(f'Connected (mode: {grid.client._mode})\n')

    # Say hello
    print(grid.hello('SOTA Client'))

    # Read grid info
    infos: list[GridAttribute] = grid.get_grid_infos(1, [0])
    attr = infos[0]
    print(f'Grid 1-0: activate={attr.activate}, level={attr.level}, '
          f'global_id={attr.global_id}, '
          f'bounds=({attr.min_x}, {attr.min_y}, {attr.max_x}, {attr.max_y})')

    # Subdivide
    keys = grid.subdivide_grids([1], [0])
    print(f'\nSubdivided 1-0 → {len(keys)} children: {keys[:4]}…')

    # Read children
    child_ids = [int(k.split('-')[1]) for k in keys]
    children = grid.get_grid_infos(2, child_ids[:4])
    for c in children:
        print(f'  Child 2-{c.global_id}: activate={c.activate}, '
              f'bounds=({c.min_x:.1f}, {c.min_y:.1f}, {c.max_x:.1f}, {c.max_y:.1f})')

    # Active grids
    levels, global_ids = grid.get_active_grid_infos()
    print(f'\nActive grids: {len(levels)} total')

    # Done
    cc.close(grid)
    print('\nClient done.')


if __name__ == '__main__':
    main()
