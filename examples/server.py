import os
import sys
import logging

logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../examples/')))

import c_two as cc
from crm import Grid
from icrm import IGrid
from example_addresses import EXAMPLE_ADDRESS

if __name__ == '__main__':
    # Grid parameters
    epsg = 2326
    first_size = [64.0, 64.0]
    bounds = [808357.5, 824117.5, 838949.5, 843957.5]
    subdivide_rules = [
        #    64x64,  32x32,  16x16,    8x8,    4x4,    2x2,    1x1
        [478, 310], [2, 2], [2, 2], [2, 2], [2, 2], [2, 2], [1, 1]
    ]
    
    # Init CRM
    grid = Grid(epsg, bounds, first_size, subdivide_rules)
    
    # Create server config
    config = cc.rpc.ServerConfig(
        name='Grid Processor',
        crm=grid,
        icrm=IGrid,
        on_shutdown=grid.terminate,
        bind_address=EXAMPLE_ADDRESS
    )
    
    # Create CRM server
    server = cc.rpc.Server(config)

    # Run CRM server
    server.start()