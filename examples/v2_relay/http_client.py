"""HTTP client — connects to the Grid CRM via the relay.

Demonstrates that the ICRM proxy works identically over HTTP as it
does over IPC — only the address changes.

Usage (3-terminal workflow):

    # Terminal 1 — start the Grid CRM
    uv run python examples/v2_relay/resource.py

    # Terminal 2 — start the HTTP relay
    uv run python examples/v2_relay/relay_server.py

    # Terminal 3 — run this client
    uv run python examples/v2_relay/http_client.py
"""
import os, sys

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../../src/')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../../examples/')))

import c_two as cc
from icrm import IGrid, GridAttribute

RELAY_URL = 'http://127.0.0.1:8080'


def main():
    # Connect via HTTP relay — same API as IPC, different address.
    grid = cc.connect(IGrid, name='grid', address=RELAY_URL)
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
    cc.shutdown()
    print('\n[Client] Done.')


if __name__ == '__main__':
    main()
