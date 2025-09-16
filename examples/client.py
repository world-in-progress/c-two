import os
import sys

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../examples/')))

import c_two as cc
from icrm import IGrid, GridAttribute
from example_addresses import EXAMPLE_ADDRESS

# Components in function style
@cc.runtime.connect
def get_grid_infos(crm: IGrid, level: int, global_ids: list[int]) -> list[GridAttribute]:
    grids: list[GridAttribute] = crm.get_grid_infos(level, global_ids)
    return grids

@cc.runtime.connect
def subdivide_grids(crm: IGrid, levels: list[int], global_ids: list[int]) -> list[str]:
    keys: list[str] = crm.subdivide_grids(levels, global_ids)
    return keys

@cc.runtime.connect
def get_parent_keys(crm: IGrid, levels: list[int], global_ids: list[int]) -> list[str | None]:
    keys: list[str] = crm.get_parent_keys(levels, global_ids)
    return keys

@cc.runtime.connect
def get_active_grid_infos(crm: IGrid) -> tuple[list[int], list[int]]:
    levels, global_ids = crm.get_active_grid_infos()
    return levels, global_ids

if __name__ == '__main__':
    # Check if CRM is running
    if cc.rpc.Client.ping(EXAMPLE_ADDRESS):
        print('CRM is running!\n')
    else:
        print('CRM is not running!\n')
        sys.exit(1)
    
    # One way to use connect_crm:
    # Provide both the address and an ICRM class.
    # connect_crm returns an ICRM instance that can be used directly.
    # This approach is particularly useful for component scripts.
    with cc.runtime.connect_crm(EXAMPLE_ADDRESS, IGrid) as grid:
        print(grid.hello('World'))
        # Check grid 1-0
        parent: GridAttribute = grid.get_grid_infos(1, [0])[0]
        print('Parent checked by ICRM instance:', parent.activate, parent.level, parent.global_id, parent.local_id, parent.elevation, parent.min_x, parent.min_y, parent.max_x, parent.max_y)
    
    # Alternative way to use connect_crm:
    # When providing only the address, connect_crm returns a Client instance.
    # While the client is rarely needed directly, it's used internally by the runtime.
    # Functions decorated with @cc.runtime.connect automatically receive an ICRM instance that contains this client as their first argument.
    # This approach is particularly useful for component functions.
    # In this case, the client is not needed, but it's included here for demonstration purposes.
    # with cc.runtime.connect_crm(EXAMPLE_ADDRESS) as client:
    with cc.runtime.connect_crm(EXAMPLE_ADDRESS):
        # Subdivide grid 1-0
        keys = subdivide_grids([1], [0])
        
        # Check grid 1-0 and children:
        parent = get_grid_infos(1, [0])[0]
        print('Parent:', parent.activate, parent.level, parent.global_id, parent.local_id, parent.elevation, parent.min_x, parent.min_y, parent.max_x, parent.max_y)

        children = get_grid_infos(2, [int(key.split('-')[1]) for key in keys])
        for child in children:
            print('Child:', child.activate, child.level, child.global_id, child.local_id, child.elevation, child.min_x, child.min_y, child.max_x, child.max_y)
            
        # Check get parents
        levels = [1] + [2] * len(keys)
        global_ids = [0] + [int(key.split('-')[1]) for key in keys]
        parents = get_parent_keys(levels, global_ids)
        for parent in parents:
            print('Parent:', parent)
        
        # Test get_active_grid_infos
        levels, global_ids = get_active_grid_infos()
        print(len(levels), len(global_ids))