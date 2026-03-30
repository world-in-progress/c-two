"""SOTA API — HTTP relay client.

Connects to the relay server started by ``relay_server.py`` via HTTP
and invokes CRM methods through the ICRM proxy — identical API to IPC.

Run (after starting relay_server.py in another terminal):
    uv run python examples/relay_client.py
"""
import os, sys

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../examples/')))

import c_two as cc
from icrm import IGrid, GridAttribute

RELAY_URL = 'http://127.0.0.1:8080'


def main():
    # Connect via HTTP — identical API to IPC, just a different address.
    grid = cc.connect(IGrid, name='grid', address=RELAY_URL)
    print(f'Connected (mode: {grid.client._mode})\n')

    # Say hello
    print(grid.hello('HTTP Relay Client'))

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
