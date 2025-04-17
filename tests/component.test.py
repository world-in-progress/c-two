import os
import sys

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '.')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
import c_two as cc

import component as com

if __name__ == '__main__':
    
    ipc_address = 'ipc:///tmp/zmq_test'
    tcp_address = 'tcp://localhost:5555'
    
    # Check if CRM is running
    if cc.message.Client.ping(tcp_address):
        print('CRM is running!\n')
    else:
        print('CRM is not running!\n')
        sys.exit(1)
    
    with cc.compo.runtime.connect_crm(tcp_address):
        # Check grid 1-0
        parent: com.GridAttribute = com.get_grid_infos(1, [0])[0]
        print('Parent:', parent.activate, parent.level, parent.global_id, parent.local_id, parent.elevation, parent.min_x, parent.min_y, parent.max_x, parent.max_y)
            
        # Subdivide grid 1-0
        keys = com.subdivide_grids([1], [0])
        
        # Check grid 1-0 and children:
        parent = com.get_grid_infos(1, [0])[0]
        print('Parent:', parent.activate, parent.level, parent.global_id, parent.local_id, parent.elevation, parent.min_x, parent.min_y, parent.max_x, parent.max_y)
        
        children = com.get_grid_infos(2, [int(key.split('-')[1]) for key in keys])
        for child in children:
            print('Child:', child.activate, child.level, child.global_id, child.local_id, child.elevation, child.min_x, child.min_y, child.max_x, child.max_y)
            
        # Check get parents
        levels = [1] + [2] * len(keys)
        global_ids = [0] + [int(key.split('-')[1]) for key in keys]
        parents = com.get_parent_keys(levels, global_ids)
        for parent in parents:
            print('Parent:', parent)
    