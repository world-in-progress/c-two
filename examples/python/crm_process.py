"""CRM resource process.

Registers CRMs and keeps the process alive so remote clients can connect
via IPC. Pass ``--relay-url`` to also announce the CRM to an HTTP relay.
Press Ctrl-C to shut down.

Run:
    uv run python examples/python/crm_process.py

Copy the printed IPC address, then in another terminal:
    uv run python examples/python/client.py <address>
"""
import argparse
import sys
from pathlib import Path

EXAMPLES_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(EXAMPLES_ROOT))

import c_two as cc
import logging
from grid.nested_grid import NestedGrid
from grid.grid_contract import Grid

logging.basicConfig(level=logging.DEBUG)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description='Start the Grid CRM process for IPC or relay-backed examples.',
    )
    parser.add_argument(
        '--relay-url',
        help='HTTP relay URL to register with, such as http://127.0.0.1:8300.',
    )
    return parser.parse_args()


def main():
    args = parse_args()
    if args.relay_url:
        cc.set_relay(args.relay_url)

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
    print(f'Grid CRM registered at {cc.server_address()}', flush=True)

    # Block until SIGINT/SIGTERM, then auto-shutdown via atexit.
    cc.serve()


if __name__ == '__main__':
    main()
