"""Grid CRM process — auto-registers with the relay.

Reuses the existing ``IGrid`` ICRM, ``Grid`` CRM, and transferable
types from ``examples/grid/``.

Run (after starting relay.py):
    uv run python examples/relay_mesh/resource.py
"""
import os, sys

# Ensure c_two and the sibling grid package are importable.
_examples_dir = os.path.dirname(os.path.dirname(__file__))
sys.path.insert(0, os.path.join(_examples_dir, '..', 'src'))
sys.path.insert(0, _examples_dir)

import c_two as cc
import logging

from grid.igrid import IGrid
from grid.grid import Grid

logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s [resource] %(levelname)s %(message)s',
    datefmt='%H:%M:%S',
)

# ── Main ────────────────────────────────────────────────────────────

def main():
    grid = Grid(
        epsg=2326,
        bounds=[808357.5, 824117.5, 838949.5, 843957.5],
        first_size=[64.0, 64.0],
        subdivide_rules=[[4, 3], [2, 2], [2, 2], [2, 2]],
    )

    cc.register(IGrid, grid, name='grid')
    print(f'Grid CRM registered (IPC: {cc.server_address()})')
    print('Press Ctrl-C to stop.\n')

    cc.serve()


if __name__ == '__main__':
    main()
