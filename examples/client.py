"""SOTA API — client process.

Connects to the server started by ``v2_server.py`` via IPC and invokes
CRM methods through the ICRM proxy.

Run (after starting v2_server.py in another terminal):
    uv run python examples/v2_client.py
"""
import os, sys
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../examples/')))

import c_two as cc
from icrm import IGrid, GridAttribute

SERVER_ADDRESS = 'ipc-v3://v2_grid'


def main():
    # Connect to the remote server via IPC
    grid = cc.connect(IGrid, name='grid', address=SERVER_ADDRESS)
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
    cc.shutdown()
    print('\nClient done.')


if __name__ == '__main__':
    main()
