"""Run POSIX fixture scripts through the host's actual Unix/Git shell."""
from __future__ import annotations

import os
from pathlib import Path
import shlex
import shutil
import sys


def shell_path(path: Path | str) -> str:
    value = str(path).replace("\\", "/")
    if os.name == "nt" and len(value) > 2 and value[1:3] == ":/":
        return "/" + value[0].lower() + value[2:]
    return value


def posix_shell(name: str = "bash") -> str:
    if os.name == "nt":
        git = shutil.which("git")
        candidates = []
        if git:
            candidates.append(Path(git).resolve().parent.parent / "bin" / f"{name}.exe")
        candidates.append(Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "Git/bin" / f"{name}.exe")
        for candidate in candidates:
            if candidate.is_file():
                return str(candidate)
        raise AssertionError("POSIX fixture tests require Git for Windows on PATH")
    executable = shutil.which(name)
    assert executable is not None, f"POSIX fixture tests require {name}"
    return executable


def shell_environment(bin_dir: Path | None = None) -> dict[str, str]:
    env = dict(os.environ)
    if os.name == "nt":
        git_bin = Path(posix_shell()).parent
        paths = [shell_path(git_bin.parent / "usr/bin"), shell_path(git_bin)]
        paths.extend(shell_path(path) for path in env.get("PATH", "").split(os.pathsep) if path)
        env["PATH"] = ":".join(paths)
    if bin_dir is not None:
        bin_dir.mkdir(parents=True, exist_ok=True)
        # Workflow snippets and fake tools must use this test interpreter even
        # on hosts where python3 is absent or python points at a Store alias.
        # Native Windows Python emits CRLF even when Bash redirects stdout.
        # POSIX xargs treats that CR as part of each filename, so the small
        # launcher gives workflow heredocs the Unix stdout convention.
        launcher = bin_dir / "posix_python.py"
        launcher.write_bytes(b'''import runpy, sys
sys.stdout.reconfigure(newline="\\n")
sys.argv.pop(0)
if sys.argv[0] == "-":
    exec(compile(sys.stdin.read(), "<stdin>", "exec"))
elif sys.argv[0] == "-c":
    source = sys.argv.pop(1)
    exec(compile(source, "<string>", "exec"))
else:
    runpy.run_path(sys.argv[0], run_name="__main__")
''')
        for name in ("python", "python3"):
            wrapper = bin_dir / name
            wrapper.write_bytes(("#!/bin/sh\nexec " + shlex.quote(shell_path(sys.executable)) + " " + shlex.quote(shell_path(launcher)) + ' "$@"\n').encode("utf-8"))
            wrapper.chmod(0o755)
        env["PATH"] = shell_path(bin_dir) + ":" + env.get("PATH", "")
    return env
