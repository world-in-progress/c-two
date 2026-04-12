"""CRM resource process.

Registers CRMs and keeps the process alive so remote clients can connect
via IPC.  Press Ctrl-C to shut down.

Run:
    uv run python examples/crm_process.py

Then in another terminal:
    uv run python examples/compo.py
"""
import os, sys

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../examples/')))

import c_two as cc
import logging
from grid.grid import Grid
from grid.igrid import IGrid

BIND_ADDRESS = 'ipc://grid'
logging.basicConfig(level=logging.DEBUG)


def main():
    # Set address before registering any CRM.
    cc.set_ipc_address(BIND_ADDRESS)

    # Init the Grid CRM (same as old server.py)
    epsg = 2326
    first_size = [64.0, 64.0]
    bounds = [808357.5, 824117.5, 838949.5, 843957.5]
    subdivide_rules = [
        [478, 310], [2, 2], [2, 2], [2, 2], [2, 2], [2, 2], [1, 1]
    ]
    grid = Grid(epsg, bounds, first_size, subdivide_rules)

    # Register — one line replaces ServerConfig + Server + start()
    cc.register(IGrid, grid, name='grid')
    print(f'Grid CRM registered at {cc.server_address()}')

    # Block until SIGINT/SIGTERM, then auto-shutdown via atexit.
    cc.serve()


if __name__ == '__main__':
    main()
