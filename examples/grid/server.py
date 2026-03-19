"""
Grid CRM Server Example
========================
Demonstrates how to start a CRM server with C-Two.

Usage:
    python -m examples.grid.server
"""
import sys
import os
import logging

logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../../src/')))
import c_two as cc

from examples.grid.crm import Grid
from examples.grid.icrm import IGrid
from examples.grid.addresses import THREAD_ADDRESS

def create_grid():
    """Create a Grid CRM with example parameters."""
    return Grid(
        epsg=2326,
        bounds=[808357.5, 824117.5, 838949.5, 843957.5],
        first_size=[64.0, 64.0],
        subdivide_rules=[[478, 310], [2, 2], [2, 2], [2, 2], [2, 2], [2, 2], [1, 1]],
    )

if __name__ == '__main__':
    grid = create_grid()

    config = cc.rpc.ServerConfig(
        name='Grid Processor',
        crm=grid,
        icrm=IGrid,
        on_shutdown=grid.terminate,
        bind_address=THREAD_ADDRESS,
    )

    server = cc.rpc.Server(config)
    server.start()
