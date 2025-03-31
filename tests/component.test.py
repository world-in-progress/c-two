import os
import sys
import c_two as cc

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '..')))

if __name__ == '__main__':
    component_service_server_module_names = ['component']
    cc.ComponentServer.serve(component_service_server_module_names)
    