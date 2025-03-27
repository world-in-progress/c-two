import os
import sys
import subprocess

class CRMServer:
    def __init__(self):
        self.crm_process: subprocess.Popen = None
        self.component_process: subprocess.Popen = None
        
    def _start_crm(self, crm_module: str, crm_class: str) -> subprocess.Popen:
        """Start the CRM serives in a separate process."""
        cmd = [sys.executable, "-m", "c_two.service", crm_module, crm_class]
        self.crm_process = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0
        )
        return self.crm_process
    
    def _start_component_script(self, component_path: str) -> subprocess.Popen:
        """Start the component script in a seperate process."""
        cmd = [sys.executable, component_path]
        env = os.environ.copy()
        self.component_process = subprocess.Popen(
            cmd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0
        )
        return self.component_process
    
    def run(self, component_path: str, crm_module: str, crm_class: str):
        """Start both CRM and Component processes."""
        crm_proc = self._start_crm(crm_module, crm_class)
        component_proc = self._start_component_script(component_path)
        
        component_proc.__crm_process__ = crm_proc
        
        component_proc.wait()
        crm_proc.wait()
