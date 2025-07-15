import os
import sys

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '.')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
import c_two as cc

import time
import logging
logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)

if __name__ == '__main__':
    
    from crm import IGrid, Grid
    import component as com
    
    THREAD_ADDRESS = 'thread://root_hello'

    TEST_ADDRESS = THREAD_ADDRESS

    # Grid parameters
    epsg = 2326
    first_size = [64.0, 64.0]
    bounds = [808357.5, 824117.5, 838949.5, 843957.5]
    subdivide_rules = [
        #    64x64,  32x32,  16x16,    8x8,    4x4,    2x2,    1x1
        [478, 310], [2, 2], [2, 2], [2, 2], [2, 2], [2, 2], [1, 1]
    ]
    
    # Init CRM
    grid_file_path='./grids.arrow'
    crm = Grid(epsg, bounds, first_size, subdivide_rules)
    
    # Create CRM server
    server = cc.rpc.Server(TEST_ADDRESS, crm)

    # Run CRM server and handle termination gracefully
    server.start()
    
    # -- Component tests --
    

    # Check if CRM is running
    if cc.rpc.Client.ping(TEST_ADDRESS):
        logger.info('CRM is running!\n')
    else:
        logger.error('CRM is not running!\n')
        sys.exit(1)
    
    # One way to use connect_crm:
    # Provide both the address and an ICRM class.
    # connect_crm returns an ICRM instance that can be used directly.
    # This approach is particularly useful for component scripts.
    with cc.compo.runtime.connect_crm(TEST_ADDRESS, IGrid) as grid:
        logger.info(grid.hello('World'))
        # Check grid 1-0
        parent: com.GridAttribute = grid.get_grid_infos(1, [0])[0]
        logger.info('Parent checked by ICRM instance: %s', parent)
        
    # Alternative way to use connect_crm:
    # When providing only the address, connect_crm returns a Client instance.
    # While the client is rarely needed directly, it's used internally by the runtime.
    # Functions decorated with @cc.compo.runtime.connect automatically receive an ICRM instance that contains this client as their first argument.
    # This approach is particularly useful for component functions.
    # In this case, the client is not needed, but it's included here for demonstration purposes.
    with cc.compo.runtime.connect_crm(TEST_ADDRESS) as client:

        # Subdivide grid 1-0
        keys = com.subdivide_grids([1], [0])
        
        # Check grid 1-0 and children:
        parent = com.get_grid_infos(1, [0])[0]
        logger.info('Parent: %s', parent)

        children = com.get_grid_infos(2, [int(key.split('-')[1]) for key in keys])
        for child in children:
            logger.info('Child: %s', child)

        # Check get parents
        levels = [1] + [2] * len(keys)
        global_ids = [0] + [int(key.split('-')[1]) for key in keys]
        parents = com.get_parent_keys(levels, global_ids)
        for parent in parents:
            logger.info('Parent: %s', parent)

        # Test get_active_grid_infos
        levels, global_ids = com.get_active_grid_infos()
        logger.info('Active grid infos - Levels: %d, Global IDs: %d', len(levels), len(global_ids))

    # Run the server and wait for termination
    while cc.rpc.Client.shutdown(TEST_ADDRESS) is False:
        logger.info('Waiting for CRM to shutdown...')
        time.sleep(1)
    logger.info('CRM shutdown successfully')