"""Client — discovers and uses the Grid resource via relay.

Connects to the Grid CRM **by name only**. The relay resolves the name
to the CRM process's IPC address automatically.

Run (after starting relay.py and resource.py):
    uv run python examples/relay_mesh/client.py
"""
import os, sys

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../../src/')))

import c_two as cc

# Import the ICRM interface from the resource module.
# In production, ICRM classes live in a shared package.
sys.path.insert(0, os.path.dirname(__file__))
from resource import IGrid

RELAY_URL = 'http://127.0.0.1:8300'


def main():
    # Tell C-Two where the relay is — all name resolution goes through it.
    cc.set_relay(RELAY_URL)

    # ── Connect by name ──────────────────────────────────────────────
    # No IPC address needed — the relay resolves 'grid' to the right
    # CRM process automatically.
    grid = cc.connect(IGrid, name='grid')
    print(f'Connected to Grid (mode: {grid.client._mode})\n')

    # ── 1. Hello ─────────────────────────────────────────────────────
    msg = grid.hello('Relay Mesh Client')
    print(f'hello → {msg}')

    # ── 2. Schema ────────────────────────────────────────────────────
    schema = grid.get_schema()
    print(f'\nGrid schema:')
    print(f'  EPSG:      {schema["epsg"]}')
    print(f'  Bounds:    {schema["bounds"]}')
    print(f'  Cell size: {schema["cell_size"]}')
    print(f'  Levels:    {schema["level_count"]}')

    # ── 3. Query grid cells ──────────────────────────────────────────
    cells = grid.get_cells(1, [0, 1, 2])
    print(f'\nLevel-1 cells (first 3):')
    for c in cells:
        print(f'  Cell 1-{c["global_id"]}: '
              f'active={c["activate"]}, '
              f'bounds=({c["min_x"]:.1f}, {c["min_y"]:.1f}, '
              f'{c["max_x"]:.1f}, {c["max_y"]:.1f})')

    # ── 4. Subdivide ─────────────────────────────────────────────────
    keys = grid.subdivide(1, 0)
    print(f'\nSubdivided cell 1-0 → {len(keys)} children')
    if keys:
        print(f'  Keys: {keys}')

    # ── 5. Query children ────────────────────────────────────────────
    if keys:
        child_ids = [int(k.split('-')[1]) for k in keys]
        children = grid.get_cells(2, child_ids)
        print(f'\nChild cells (level 2):')
        for c in children:
            print(f'  Cell 2-{c["global_id"]}: '
                  f'bounds=({c["min_x"]:.1f}, {c["min_y"]:.1f}, '
                  f'{c["max_x"]:.1f}, {c["max_y"]:.1f})')

    # ── 6. Active cell count ─────────────────────────────────────────
    count = grid.active_count()
    print(f'\nActive cells: {count}')

    # ── Cleanup ──────────────────────────────────────────────────────
    cc.close(grid)
    print('\nDone.')


if __name__ == '__main__':
    main()
