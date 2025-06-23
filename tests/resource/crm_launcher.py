import os
import sys
import logging
import subprocess
from pathlib import Path

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../../src/')))

import c_two as cc

logging.basicConfig(level=logging.DEBUG, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)

TEST_DIR = Path(os.getcwd()).resolve() / 'tests'
MOCK_CRM_LAUNCHER_PY = TEST_DIR / 'crm.test.py'
CRM_PROCESS: subprocess.Popen = None
    
MEMORY_ADDRESS = 'memory://test'
IPC_ADDRESS = 'ipc:///tmp/zmq_test'
TCP_ADDRESS = 'tcp://localhost:5555'
HTTP_ADDRESS = 'http://localhost:5556'

TEST_ADDRESS = MEMORY_ADDRESS

def start_mock_crm():
    global CRM_PROCESS
    # Platform-specific subprocess arguments
    kwargs = {}
    if sys.platform != 'win32':
        # Unix-specific: create new process group
        kwargs['preexec_fn'] = os.setsid
    
    cmd = [
        sys.executable,
        MOCK_CRM_LAUNCHER_PY,
    ]
    
    CRM_PROCESS = subprocess.Popen(
        cmd,
        **kwargs
    )

    while cc.rpc.Client.ping(TEST_ADDRESS) is False:
        pass

    logger.info(f'Mock CRM process started with PID: {CRM_PROCESS.pid}')

def stop_mock_crm():
    global CRM_PROCESS
    if cc.rpc.Client.shutdown(TEST_ADDRESS, 0.5):
        
        logger.info(f'Mock CRM process stopped with PID: {CRM_PROCESS.pid}')
    else:
        logger.error('Failed to stop the Mock CRM process.')

def stop_mock_crm_by_process():
    global CRM_PROCESS
    if cc.rpc.Client.shutdown_by_process(CRM_PROCESS, 2.0):
        logger.info(f'Mock CRM process stopped with PID: {CRM_PROCESS.pid}')
    else:
        logger.error('Failed to stop the Mock CRM process.')

if __name__ == '__main__':
    start_mock_crm()
    stop_mock_crm_by_process()
