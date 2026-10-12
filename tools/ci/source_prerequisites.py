"""Build pinned WASM and package already-tested Node runtime bytes once per job."""

from __future__ import annotations

import argparse
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from tools.ci.source_validation import ROOT, npm_command


def run(command: list[str], *, cwd: Path, **kwargs):
    return subprocess.run(command, cwd=cwd, check=True, **kwargs)


def wasm_cache() -> Path:
    return ROOT.parent / "fastdb-wasm"


def wasm() -> None:
    fastdb = ROOT.parent / "fastdb"
    cache = wasm_cache()
    if not all((cache / name).is_file() for name in ("fastdb4ts.js", "fastdb4ts.wasm")):
        emcmake = Path(os.environ["EMSDK"]) / "upstream/emscripten/emcmake.py"
        environment = os.environ.copy()
        for key in ("CMAKE_C_COMPILER_LAUNCHER", "CMAKE_CXX_COMPILER_LAUNCHER"):
            environment.pop(key, None)
        # CMake writes its JS loader beside the WASM. Windows emits different
        # glue, so build an immutable source snapshot without changing the
        # sibling checkout used by Core and Python. No additional Git clone.
        with tempfile.TemporaryDirectory(prefix="c2-source-wasm-") as temporary:
            stage = Path(temporary) / "source"
            stage.mkdir()
            archive = run(["git", "archive", "--format=tar", "HEAD"], cwd=fastdb, capture_output=True)
            with tarfile.open(fileobj=io.BytesIO(archive.stdout)) as files:
                files.extractall(stage, filter="data")
            build = Path(temporary) / "build"
            run([sys.executable, str(emcmake), "cmake", "-S", str(stage / "ts/embind"),
                 "-B", str(build), "-G", "Ninja", "-DCMAKE_BUILD_TYPE=Release"], cwd=stage, env=environment)
            run(["cmake", "--build", str(build), "--target", "fastdb4ts", "--config", "Release"], cwd=stage, env=environment)
            generated = stage / "ts/fastdb4ts/src/wasm"
            cache.mkdir(parents=True, exist_ok=True)
            for name in ("fastdb4ts.js", "fastdb4ts.wasm"):
                shutil.copy2(generated / name, cache / name)
    run(["git", "diff", "--exit-code", "--", "ts/fastdb4ts/src/wasm"], cwd=fastdb)


def fastdb_package() -> None:
    package = ROOT.parent / "fastdb/ts/fastdb4ts"
    command = npm_command("run", "build", "--prefix", str(package))
    if os.name == "nt":
        shell = os.environ.get("C2_FASTDB_NPM_SCRIPT_SHELL") or str(
            Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "Git/bin/bash.exe")
        command.extend(["--script-shell", shell])
    run(command, cwd=ROOT)
    # Use the loader generated with these exact WASM bytes. npm's normal build
    # copied the checked-in loader; leave that tracked source untouched.
    for name in ("fastdb4ts.js", "fastdb4ts.wasm"):
        shutil.copy2(wasm_cache() / name, package / "dist/wasm" / name)


def packages(output: Path) -> None:
    output.mkdir(parents=True, exist_ok=False)
    for name, package in (
        ("fastdb4ts", str(ROOT.parent / "fastdb/ts/fastdb4ts")),
        ("c2-mem-ffi", str(ROOT / "core/foundation/c2-mem-ffi/bindings/typescript")),
        ("typescript", "typescript@5.9.3"),
    ):
        with tempfile.TemporaryDirectory(prefix="c2-source-pack-") as temporary:
            directory = Path(temporary)
            result = run(npm_command("pack", package, "--ignore-scripts", "--json"),
                         cwd=directory, capture_output=True, text=True)
            entries = json.loads(result.stdout)
            if len(entries) != 1:
                raise RuntimeError(f"Expected one {name} tarball, got {entries!r}")
            archive = directory / entries[0]["filename"]
            if archive.parent != directory or not archive.is_file():
                raise RuntimeError(f"Invalid npm pack output: {archive}")
            shutil.copy2(archive, output / f"{name}.tgz")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("kind", choices=("wasm", "fastdb", "packages"))
    parser.add_argument("--output", type=Path)
    options = parser.parse_args()
    if options.kind == "wasm":
        wasm()
    elif options.kind == "fastdb":
        fastdb_package()
    elif options.output is None:
        parser.error("packages requires --output")
    else:
        packages(options.output.resolve())
