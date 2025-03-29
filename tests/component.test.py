import os
import sys
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '..')))

if __name__ == '__main__':
    
    from c_two import ComponentClient, get_wrapper
    from proto.schema import schema_pb2 as schema
    from grid_test import GridAttribute
    
    client = ComponentClient('ipc:///tmp/zmq_test')
    
    query_args: schema.PeerGridInfos = get_wrapper(schema.PeerGridInfos).forward(1, [i for i in range (10)])
    res_proto = client.call('get_grid_infos', query_args)
    grids: list[GridAttribute] = get_wrapper(schema.GridAttributes).inverse(res_proto)
    
    for grid in grids:
        print(grid.activate, grid.deleted, grid.level, grid.global_id, grid.local_id, grid.type, grid.elevation, grid.min_x, grid.min_y, grid.max_x, grid.min_y)
    