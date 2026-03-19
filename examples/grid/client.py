"""
Grid Component Client Example
===============================
Demonstrates two styles of connecting to a CRM server:
1. Script-based: using connect_crm() context manager with ICRM class
2. Function-based: using @runtime.connect decorated functions

Usage:
    # Start server first, then:
    python -m examples.grid.client
"""
import sys
import os
import logging

logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../../src/')))
import c_two as cc

from examples.grid.icrm import IGrid, GridAttribute
from examples.grid.addresses import THREAD_ADDRESS
from examples.grid import component as com

if __name__ == '__main__':
    address = THREAD_ADDRESS

    # Check if CRM is running
    if not cc.rpc.Client.ping(address):
        logger.error('CRM is not running!')
        sys.exit(1)
    logger.info('CRM is running!')

    # Style 1: Script-based with ICRM instance
    with cc.compo.runtime.connect_crm(address, IGrid) as grid:
        parent: GridAttribute = grid.get_grid_infos(1, [0])[0]
        logger.info('Parent: level=%d, global_id=%d, activate=%s', parent.level, parent.global_id, parent.activate)

    # Style 2: Function-based with @runtime.connect
    with cc.compo.runtime.connect_crm(address):
        # Subdivide grid 1-0
        keys = com.subdivide_grids([1], [0])
        logger.info('Subdivided into %d children', len(keys))

        # Check children
        children = com.get_grid_infos(2, [int(key.split('-')[1]) for key in keys])
        for child in children:
            logger.info('Child: level=%d, global_id=%d', child.level, child.global_id)

        # Get parent keys
        levels = [1] + [2] * len(keys)
        global_ids = [0] + [int(key.split('-')[1]) for key in keys]
        parents = com.get_parent_keys(levels, global_ids)
        logger.info('Parent keys: %s', parents[:5])

        # Get active grid infos
        active_levels, active_ids = com.get_active_grid_infos()
        logger.info('Active grids: %d', len(active_levels))
