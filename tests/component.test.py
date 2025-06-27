import os
import sys

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '.')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
import c_two as cc

# import component as com
from ihello import IHello

if __name__ == '__main__':
    
    MEMORY_ADDRESS = 'memory://root_hello'
    IPC_ADDRESS = 'ipc:///tmp/zmq_test'
    TCP_ADDRESS = 'tcp://localhost:5555'
    HTTP_ADDRESS = 'http://localhost:5556'
    HTTP_ADDRESS = 'http://localhost:5556/hahaha'
    HTTP_ADDRESS = 'http://localhost:5556/?node-key=tempParentPath_tempChildPath'
    HTTP_ADDRESS = 'http://localhost:5556/api/?node-key=tempParentPath_tempChildPath'

    TEST_ADDRESS = MEMORY_ADDRESS

    # Check if CRM is running
    if cc.rpc.Client.ping(TEST_ADDRESS):
        print('CRM is running!\n')
    else:
        print('CRM is not running!\n')
        sys.exit(1)
    
    # One way to use connect_crm:
    # Provide both the address and an ICRM class.
    # connect_crm returns an ICRM instance that can be used directly.
    # This approach is particularly useful for component scripts.
    with cc.compo.runtime.connect_crm(TEST_ADDRESS, IHello) as hello:
        # hello.no_hello('world')
        print(hello.greeting('haha'))

        # print(hello.hello('World'))
    #     # Check grid 1-0
    #     parent: com.GridAttribute = grid.get_grid_infos(1, [0])[0]
    #     print('Parent checked by ICRM instance:', parent.activate, parent.level, parent.global_id, parent.local_id, parent.elevation, parent.min_x, parent.min_y, parent.max_x, parent.max_y)
    
    # # Alternative way to use connect_crm:
    # # When providing only the address, connect_crm returns a Client instance.
    # # While the client is rarely needed directly, it's used internally by the runtime.
    # # Functions decorated with @cc.compo.runtime.connect automatically receive an ICRM instance that contains this client as their first argument.
    # # This approach is particularly useful for component functions.
    # # In this case, the client is not needed, but it's included here for demonstration purposes.
    # with cc.compo.runtime.connect_crm(TEST_ADDRESS) as client:

    #     # Subdivide grid 1-0
    #     keys = com.subdivide_grids([1], [0])
        
    #     # Check grid 1-0 and children:
    #     parent = com.get_grid_infos(1, [0])[0]
    #     print('Parent:', parent.activate, parent.level, parent.global_id, parent.local_id, parent.elevation, parent.min_x, parent.min_y, parent.max_x, parent.max_y)
        
    #     children = com.get_grid_infos(2, [int(key.split('-')[1]) for key in keys])
    #     for child in children:
    #         print('Child:', child.activate, child.level, child.global_id, child.local_id, child.elevation, child.min_x, child.min_y, child.max_x, child.max_y)
            
    #     # Check get parents
    #     levels = [1] + [2] * len(keys)
    #     global_ids = [0] + [int(key.split('-')[1]) for key in keys]
    #     parents = com.get_parent_keys(levels, global_ids)
    #     for parent in parents:
    #         print('Parent:', parent)
        
    #     # Test get_active_grid_infos
    #     levels, global_ids = com.get_active_grid_infos()
    #     print(len(levels), len(global_ids))
    