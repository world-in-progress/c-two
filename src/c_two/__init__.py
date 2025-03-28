import os
import sys
import subprocess

class CRMServer:
    def __init__(self, crm_module: str, crm_class: str, poetry_env_path: str):
        self.crm_module = crm_module
        self.crm_class = crm_class
        self.crm_proc = self._start_crm(crm_module, crm_class, poetry_env_path)
        self.component_proc_map: dict[str, subprocess.Popen] = {}

    def _start_crm(self, crm_module: str, crm_class: str, poetry_env_path: str) -> subprocess.Popen:
        """Start the CRM service in a separate process using Poetry environment."""
        python_path = os.path.join(poetry_env_path, "bin", "python")
        if not os.path.exists(python_path):
            raise FileNotFoundError(f"Poetry Python executable not found at {python_path}")
        
        cmd = [python_path, "-m", "c_two.service", crm_module, crm_class]
        env = os.environ.copy()
        env["PYTHONPATH"] = f"/app/resource/crm{os.pathsep}{env.get('PYTHONPATH', '')}"
        proc = subprocess.Popen(
            cmd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0
        )
        # Check if process started successfully
        if proc.poll() is not None:
            error = proc.stderr.read().decode().strip()
            raise RuntimeError(f"Failed to start CRM process: {error}")
        return proc

    def _start_component(self, component_path: str, component_args: list[str] = None) -> subprocess.Popen:
        """Start the component script in a separate process with optional arguments."""
        cmd = [sys.executable, component_path]
        if component_args:
            cmd.extend(component_args)
        env = os.environ.copy()
        proc = subprocess.Popen(
            cmd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0
        )
        if proc.poll() is not None:
            error = proc.stderr.read().decode().strip()
            raise RuntimeError(f"Failed to start Component process: {error}")
        return proc

    def invoke_component(self, uuid: str, component_path: str, component_args: list[str] = None) -> tuple[bool, str, str]:
        """Start a Component process, wait for it to finish, and return its result."""
        if uuid in self.component_proc_map:
            raise ValueError(f"Component with UUID {uuid} already running")
        
        component_proc = self._start_component(component_path, component_args)
        component_proc.__crm_process__ = self.crm_proc
        self.component_proc_map[uuid] = component_proc

        # Wait for Component to finish and capture output
        component_proc.wait()
        stdout_output = component_proc.stdout.read().decode().strip()
        stderr_output = component_proc.stderr.read().decode().strip()
        
        # Clean up Component resources
        component_proc.stdin.close()
        component_proc.stdout.close()
        component_proc.stderr.close()
        del self.component_proc_map[uuid]
        
        return component_proc.returncode == 0, stderr_output, stdout_output

    def stop(self):
        """Stop the CRM and any running Component processes gracefully."""
        if self.crm_proc and self.crm_proc.poll() is None:
            self.crm_proc.terminate()
            try:
                self.crm_proc.wait(timeout=5)
                print("CRM process gracefully stopped.")
            except subprocess.TimeoutExpired:
                self.crm_proc.kill()
                print("CRM process forcibly killed.")
            finally:
                self.crm_proc.stdin.close()
                self.crm_proc.stdout.close()
                self.crm_proc.stderr.close()
        
        for uuid, comp_proc in list(self.component_proc_map.items()):
            if comp_proc.poll() is None:
                comp_proc.terminate()
                try:
                    comp_proc.wait(timeout=5)
                    print(f"Component {uuid} gracefully stopped.")
                except subprocess.TimeoutExpired:
                    comp_proc.kill()
                    print(f"Component {uuid} forcibly killed.")
                finally:
                    comp_proc.stdin.close()
                    comp_proc.stdout.close()
                    comp_proc.stderr.close()
            del self.component_proc_map[uuid]

# Helper ##################################################
def get_poetry_env_path(crm_project_path: str) -> str:
    """Get the Poetry virtual environment path for the CRM project."""
    try:
        result = subprocess.run(
            ["poetry", "env", "info", "--path"],
            cwd=crm_project_path,
            capture_output=True,
            text=True,
            check=True
        )
        return result.stdout.strip()
    except subprocess.CalledProcessError:
        raise RuntimeError("Poetry not installed or CRM project path invalid")