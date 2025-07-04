import os
import sys

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '.')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
import c_two as cc

import logging
logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')

if __name__ == '__main__':
    
    from crm import Grid
    # from hello import Hello
    
    MEMORY_ADDRESS = 'memory://root_hello'
    IPC_ADDRESS = 'ipc:///tmp/zmq_test'
    TCP_ADDRESS = 'tcp://localhost:5555'
    HTTP_ADDRESS = 'http://localhost:5556/?node-key=tempParentPath_tempChildPath'
    HTTP_ADDRESS = 'http://localhost:5556/hahaha'
    HTTP_ADDRESS = 'http://localhost:5556'
    HTTP_ADDRESS = 'http://localhost:5556/api/?node-key=tempParentPath_tempChildPath'

    TEST_ADDRESS = MEMORY_ADDRESS

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
    # crm = Hello()
    
    # Create CRM server
    server = cc.rpc.Server(TEST_ADDRESS, crm)

    # Run CRM server and handle termination gracefully
    server.start()
    print('CRM server is running. Press Ctrl+C to stop.')
    try:
        server.wait_for_termination()
        print('Timeout reached, stopping CRM server...')
        
    except KeyboardInterrupt:
        print('Stopping CRM server...')

    finally:
        server.stop()
        print('CRM server stopped.')
