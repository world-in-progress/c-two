"""Run embedded-Python Rust tests with this installed SDK's interpreter paths."""
from pathlib import Path
import os
import subprocess
import sys


if __name__ == '__main__':
    root = Path(__file__).resolve().parents[2]
    env = dict(os.environ, PYO3_PYTHON=sys.executable, PYTHONHOME=sys.base_prefix,
               PYTHONPATH=os.pathsep.join(sys.path))
    if os.name == 'nt':
        env['PATH'] = sys.base_prefix + os.pathsep + env.get('PATH', '')
    raise SystemExit(subprocess.call(
        ['cargo', 'test', '--locked', '--manifest-path',
         str(root / 'sdk/python/native/Cargo.toml'), *sys.argv[1:]],
        cwd=root, env=env,
    ))
