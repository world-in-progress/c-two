"""Direct IPC Grid resource process.

Run:
    uv run python examples/python/ipc_resource.py

Copy the printed IPC address, then in another terminal:
    uv run python examples/python/ipc_client.py ipc://...
"""
# from __future__ import annotations

import logging
import sys
from pathlib import Path

EXAMPLES_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(EXAMPLES_ROOT))

import c_two as cc
from grid.nested_grid import NestedGrid
from grid.grid_py_crm import GridPython

logging.basicConfig(level=logging.INFO)


def main() -> None:
    grid = NestedGrid(
        2326,
        [808357.5, 824117.5, 838949.5, 843957.5],
        [64.0, 64.0],
        [[478, 310], [2, 2], [2, 2], [2, 2], [2, 2], [2, 2], [1, 1]],
    )

    cc.register(GridPython, grid, name='examples/grid')
    print(f'Grid CRM registered at {cc.server_address()}', flush=True)
    cc.serve()


if __name__ == '__main__':
    main()
