"""HTTP client — connects to the Grid CRM via the relay.

Demonstrates that the CRM proxy works identically over HTTP as it
does over IPC — only the address changes.

Usage (3-terminal workflow):

    # Terminal 1 — start the Grid CRM
    uv run python examples/python/crm_process.py

    # Terminal 2 — start the HTTP relay
    c3 relay -b 0.0.0.0:8300

    # Terminal 3 — run this client
    uv run python examples/python/relay_client.py
"""
import sys
from pathlib import Path

EXAMPLES_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(EXAMPLES_ROOT))

import c_two as cc
from grid.grid_contract import Grid, GridAttribute


def main():
    # Connect via HTTP relay — same API as IPC, different address.
    grid = cc.connect(Grid, name='examples/grid')
    print(f'[Client] Connected via HTTP (mode: {grid.client._mode})\n')

    # ── Hello ─────────────────────────────────────────────────────
    msg = grid.hello('HTTP Client (standalone relay)')
    print(f'[Client] hello → {msg}')

    # ── Grid info ─────────────────────────────────────────────────
    infos: list[GridAttribute] = grid.get_grid_infos(1, [0])
    attr = infos[0]
    print(f'[Client] Grid 1-0: activate={attr.activate}, level={attr.level}, '
          f'global_id={attr.global_id}, '
          f'bounds=({attr.min_x}, {attr.min_y}, {attr.max_x}, {attr.max_y})')

    # ── Subdivide ─────────────────────────────────────────────────
    keys = grid.subdivide_grids([1], [0])
    print(f'[Client] Subdivided 1-0 → {len(keys)} children: {keys[:4]}…')

    child_ids = [int(k.split('-')[1]) for k in keys]
    children = grid.get_grid_infos(2, child_ids[:4])
    for c in children:
        print(f'  Child 2-{c.global_id}: '
              f'bounds=({c.min_x:.1f}, {c.min_y:.1f}, {c.max_x:.1f}, {c.max_y:.1f})')

    # ── Active grids ──────────────────────────────────────────────
    levels, global_ids = grid.get_active_grid_infos()
    print(f'[Client] Active grids: {len(levels)} total')

    # ── Cleanup ───────────────────────────────────────────────────
    cc.close(grid)
    print('\n[Client] Done.')


if __name__ == '__main__':
    main()
