import os
import sys
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '..')))

if __name__ == '__main__':
    
    from proto.schema import schema_pb2 as schema
    from c_two import CRMServer, get_wrapper
    from grid_test import CRM

    # Grid parameters
    redis_host = 'localhost'
    redis_port = 6379
    epsg = 2326
    first_size = [64.0, 64.0]
    bounds = [808357.5, 824117.5, 838949.5, 843957.5]
    subdivide_rules = [
        #    64x64,  32x32,  16x16,    8x8,    4x4,    2x2,    1x1
        [478, 310], [2, 2], [2, 2], [2, 2], [2, 2], [2, 2], [1, 1]
    ]
    
    crm: CRM
    init_args: schema.InitParams = get_wrapper(schema.InitParams).forward(redis_host, redis_port, epsg, bounds, first_size, subdivide_rules)
    crm, _ = CRM.create(init_args.SerializeToString())
    print(crm.epsg)
    
    server = CRMServer(crm, 'ipc:///tmp/zmq_test')
    server.run_service()
    