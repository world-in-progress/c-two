"""CRM resource process.

Registers CRMs and keeps the process alive so remote clients can connect
via IPC.  Press Ctrl-C to shut down.

Run:
    uv run python examples/python/crm_process.py

Then in another terminal:
    uv run python examples/python/client.py
"""
import sys
from pathlib import Path

EXAMPLES_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(EXAMPLES_ROOT))

import c_two as cc
import logging
from grid.nested_grid import NestedGrid
from grid.grid_contract import Grid

logging.basicConfig(level=logging.DEBUG)

def main():
    # Init the Grid CRM (same as old server.py)
    epsg = 2326
    first_size = [64.0, 64.0]
    bounds = [808357.5, 824117.5, 838949.5, 843957.5]
    subdivide_rules = [
        [478, 310], [2, 2], [2, 2], [2, 2], [2, 2], [2, 2], [1, 1]
    ]
    grid = NestedGrid(epsg, bounds, first_size, subdivide_rules)

    # Register — one line replaces ServerConfig + Server + start()
    cc.register(Grid, grid, name='examples/grid')
    print(f'Grid CRM registered at {cc.server_address()}')

    # Block until SIGINT/SIGTERM, then auto-shutdown via atexit.
    cc.serve()


if __name__ == '__main__':
    main()
