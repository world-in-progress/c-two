import os
import sys
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '..')))

if __name__ == '__main__':
    
    import c_two as cc
    from crm import Grid

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
    
    # Run CRM server
    ipc_address = 'ipc:///tmp/zmq_test'
    tcp_address = 'tcp://localhost:5555'
    server = cc.message.Server(tcp_address, crm)
    server.run()
    