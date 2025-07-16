import logging
import argparse
import c_two as cc
from crm import Grid

logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description='Grid Launcher')
    parser.add_argument('--server_address', type=str, required=True, help='Address for the server')
    parser.add_argument('--duration', type=float, default=None, help='Timeout for the server to start (in seconds)')
    args = parser.parse_args()

    DURATION = args.duration
    SERVER_ADDRESS = args.server_address

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
    server = cc.rpc.Server(SERVER_ADDRESS, crm)

    # Run CRM server and handle termination gracefully
    server.start()
    print('CRM server is running. Press Ctrl+C to stop.')
    try:
        server.wait_for_termination(DURATION)
        print('Timeout reached, stopping CRM server...')
        
    except KeyboardInterrupt:
        print('Stopping CRM server...')

    finally:
        server.stop()
        print('CRM server stopped.')