"""Direct IPC Grid client.

Run after starting ``ipc_resource.py``:
    uv run python examples/python/ipc_client.py ipc://...
"""
from __future__ import annotations

import argparse
import sys
from pathlib import Path

EXAMPLES_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(EXAMPLES_ROOT))

import c_two as cc
from grid.grid_py_crm import GridAttribute, GridPython


def _ipc_address(value: str) -> str:
    value = value.strip()
    if not value.startswith('ipc://'):
        raise argparse.ArgumentTypeError('address must start with ipc://')
    return value


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description='Connect to the Grid CRM through direct IPC.',
    )
    parser.add_argument(
        'address',
        type=_ipc_address,
        help='Server IPC address printed by examples/python/ipc_resource.py, such as ipc://...',
    )
    return parser.parse_args(argv)


def main() -> None:
    args = parse_args()
    grid = cc.connect(GridPython, name='examples/grid', address=args.address)
    print(f'Connected (mode: {grid.client._mode})\n')

    print(grid.hello('IPC Client'))

    infos: list[GridAttribute] = grid.get_grid_infos(1, [0])
    attr = infos[0]
    print(f'Grid 1-0: activate={attr.activate}, level={attr.level}, '
          f'global_id={attr.global_id}, '
          f'bounds=({attr.min_x}, {attr.min_y}, {attr.max_x}, {attr.max_y})')

    keys = grid.subdivide_grids([1], [0])
    print(f'\nSubdivided 1-0 -> {len(keys)} children: {keys[:4]}')

    child_ids = [int(k.split('-')[1]) for k in keys]
    children = grid.get_grid_infos(2, child_ids[:4])
    for child in children:
        print(f'  Child 2-{child.global_id}: activate={child.activate}, '
              f'bounds=({child.min_x:.1f}, {child.min_y:.1f}, '
              f'{child.max_x:.1f}, {child.max_y:.1f})')

    levels, _global_ids = grid.get_active_grid_infos()
    print(f'\nActive grids: {len(levels)} total')

    cc.close(grid)
    print('\nIPC client done.')


if __name__ == '__main__':
    main()
