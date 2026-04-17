"""Client — discovers and uses the Grid resource via relay.

Connects to the Grid CRM **by name only**. The relay resolves the name
to the CRM process's IPC address automatically.

Run (after starting relay.py and resource.py):
    uv run python examples/relay_mesh/client.py
"""
import os, sys

# Ensure c_two and the sibling grid package are importable.
_examples_dir = os.path.dirname(os.path.dirname(__file__))
sys.path.insert(0, os.path.join(_examples_dir, '..', 'src'))
sys.path.insert(0, _examples_dir)

import c_two as cc

# Import the CRM contract from the shared grid package.
from grid.grid_contract import Grid

# RELAY_URL = 'http://127.0.0.1:8300'


def main():
    # Tell C-Two where the relay is — all name resolution goes through it.
    # cc.set_relay(RELAY_URL)

    # ── Connect by name ──────────────────────────────────────────────
    # No IPC address needed — the relay resolves 'grid' to the right
    # CRM process automatically.
    grid = cc.connect(Grid, name='grid')
    print(f'Connected to Grid (mode: {grid.client._mode})\n')

    # ── 1. Hello ─────────────────────────────────────────────────────
    msg = grid.hello('Relay Mesh Client')
    print(f'hello → {msg}')

    # ── 2. Schema ────────────────────────────────────────────────────
    schema = grid.get_schema()
    print(f'\nGrid schema:')
    print(f'  EPSG:      {schema.epsg}')
    print(f'  Bounds:    {schema.bounds}')
    print(f'  Cell size: {schema.first_size}')
    print(f'  Rules:     {schema.subdivide_rules}')

    # ── 3. Query grid cells ──────────────────────────────────────────
    cells = grid.get_grid_infos(1, [0, 1, 2])
    print(f'\nLevel-1 cells (first 3):')
    for c in cells:
        print(f'  Cell 1-{c.global_id}: '
              f'active={c.activate}, '
              f'bounds=({c.min_x:.1f}, {c.min_y:.1f}, '
              f'{c.max_x:.1f}, {c.max_y:.1f})')

    # ── 4. Subdivide ─────────────────────────────────────────────────
    keys = grid.subdivide_grids([1], [0])
    print(f'\nSubdivided cell 1-0 → {len(keys)} children')
    if keys:
        print(f'  Keys: {keys}')

    # ── 5. Query children ────────────────────────────────────────────
    if keys:
        child_ids = [int(k.split('-')[1]) for k in keys]
        children = grid.get_grid_infos(2, child_ids)
        print(f'\nChild cells (level 2):')
        for c in children:
            print(f'  Cell 2-{c.global_id}: '
                  f'bounds=({c.min_x:.1f}, {c.min_y:.1f}, '
                  f'{c.max_x:.1f}, {c.max_y:.1f})')

    # ── 6. Active cell count ─────────────────────────────────────────
    levels, gids = grid.get_active_grid_infos()
    print(f'\nActive cells: {len(levels)}')

    # ── Cleanup ──────────────────────────────────────────────────────
    cc.close(grid)
    print('\nDone.')


if __name__ == '__main__':
    main()
