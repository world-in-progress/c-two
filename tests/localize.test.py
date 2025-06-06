import os
import sys

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '.')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
import c_two as cc

if __name__ == '__main__':
    from crm import Grid
    from icrm import IGrid
    
    # Grid parameters
    params = {
        'epsg': 2326,
        'first_size': [64.0, 64.0],
        'bounds': [808357.5, 824117.5, 838949.5, 843957.5],
        'subdivide_rules': [
            #    64x64,  32x32,  16x16,    8x8,    4x4,    2x2,    1x1
            [478, 310], [2, 2], [2, 2], [2, 2], [2, 2], [2, 2], [1, 1]
        ]
    }
    
    # Init CRM
    grid = Grid(**params)
    
    with cc.compo.runtime.connect_crm(grid, IGrid) as igrid:
        # Check grid 1-0
        parent = igrid.get_grid_infos(1, [0])[0]
        print('Parent checked by ICRM instance:', parent.activate, parent.level, parent.global_id, parent.local_id, parent.elevation, parent.min_x, parent.min_y, parent.max_x, parent.max_y)
        
        # Test hello method
        print(igrid.hello(name='World'))
        