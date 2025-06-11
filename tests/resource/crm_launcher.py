import os
import sys
import signal
import logging
import subprocess
from pathlib import Path

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../../src/')))

import c_two as cc

logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)

TEST_RESOURCE_DIR = Path(os.getcwd()).resolve() / 'tests' / 'resource'
MOCK_CRM_LAUNCHER_PY = TEST_RESOURCE_DIR / 'mock.crm.py'
CRM_PROCESS: subprocess.Popen = None

def start_mock_crm():
    global CRM_PROCESS
    # Platform-specific subprocess arguments
    kwargs = {}
    if sys.platform != 'win32':
        # Unix-specific: create new process group
        kwargs['preexec_fn'] = os.setsid
    else:
        # Windows-specific: don't open a new console window
        kwargs['creationflags'] = subprocess.CREATE_NEW_PROCESS_GROUP
    
    cmd = [
        sys.executable,
        MOCK_CRM_LAUNCHER_PY,
    ]
    
    CRM_PROCESS = subprocess.Popen(
        cmd,
        **kwargs
    )

    while cc.message.Client.ping('tcp://localhost:5555') is False:
        pass

    logger.info(f"Mock CRM process started with PID: {CRM_PROCESS.pid}")

def stop_mock_crm():
    global CRM_PROCESS
    if CRM_PROCESS:
        if sys.platform != 'win32':
            # Unix-specific: terminate the process group
            try:
                os.killpg(os.getpgid(CRM_PROCESS.pid), signal.SIGINT)
            except (AttributeError, ProcessLookupError):
                CRM_PROCESS.terminate()
        else:
            # Windows-specific: send Ctrl+C signal and then terminate
            try:
                CRM_PROCESS.send_signal(signal.CTRL_C_EVENT)
            except (AttributeError, ProcessLookupError):
                CRM_PROCESS.terminate()

        try:
            CRM_PROCESS.wait(timeout=5)
        except subprocess.TimeoutExpired:
            if sys.platform != 'win32':
                try:
                    os.killpg(os.getpgid(CRM_PROCESS.pid), signal.SIGKILL)
                except (AttributeError, ProcessLookupError):
                    CRM_PROCESS.kill()
            else:
                CRM_PROCESS.kill()
        
        logger.info(f"Mock CRM process stopped with PID: {CRM_PROCESS.pid}")
    else:
        logger.warning("Mock CRM process is not running.")

if __name__ == '__main__':
    stop_mock_crm()
    start_mock_crm()
    stop_mock_crm()
