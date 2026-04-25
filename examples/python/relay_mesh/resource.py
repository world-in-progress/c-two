"""Grid CRM process — auto-registers with the relay.

Reuses the existing ``Grid`` contract, ``NestedGrid`` resource, and transferable
types from ``examples/python/grid/``.

Run (after starting relay.py):
    uv run python examples/python/relay_mesh/resource.py
"""
import os
import sys
from pathlib import Path

EXAMPLES_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(EXAMPLES_ROOT))

import c_two as cc
import logging

from grid.grid_contract import Grid
from grid.nested_grid import NestedGrid

RELAY_URL = os.environ.get('C2_RELAY_ADDRESS', 'http://127.0.0.1:8300')

logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s [resource] %(levelname)s %(message)s',
    datefmt='%H:%M:%S',
)

# ── Main ────────────────────────────────────────────────────────────

def main():
    cc.set_relay(RELAY_URL)

    grid = NestedGrid(
        epsg=2326,
        bounds=[808357.5, 824117.5, 838949.5, 843957.5],
        first_size=[64.0, 64.0],
        subdivide_rules=[[4, 3], [2, 2], [2, 2], [2, 2]],
    )

    cc.register(Grid, grid, name='grid')
    print(f'Grid CRM registered (IPC: {cc.server_address()})')
    print('Press Ctrl-C to stop.\n')

    cc.serve()


if __name__ == '__main__':
    main()
