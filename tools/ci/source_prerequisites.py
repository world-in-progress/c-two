"""Build pinned WASM and package already-tested Node runtime bytes once per job."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from source_validation import ROOT, npm_command


def run(command: list[str], *, cwd: Path, **kwargs):
    return subprocess.run(command, cwd=cwd, check=True, **kwargs)


def wasm() -> None:
    fastdb = ROOT.parent / "fastdb"
    package = fastdb / "ts/fastdb4ts"
    if not (package / "src/wasm/fastdb4ts.wasm").is_file():
        emcmake = Path(os.environ["EMSDK"]) / "upstream/emscripten/emcmake.py"
        environment = os.environ.copy()
        for key in ("CMAKE_C_COMPILER_LAUNCHER", "CMAKE_CXX_COMPILER_LAUNCHER"):
            environment.pop(key, None)
        with tempfile.TemporaryDirectory(prefix="c2-source-wasm-") as temporary:
            build = Path(temporary)
            run([sys.executable, str(emcmake), "cmake", "-S", str(fastdb / "ts/embind"),
                 "-B", str(build), "-G", "Ninja", "-DCMAKE_BUILD_TYPE=Release"], cwd=package, env=environment)
            run(["cmake", "--build", str(build), "--target", "fastdb4ts", "--config", "Release"], cwd=package, env=environment)
    run(["git", "diff", "--exit-code", "--", "ts/fastdb4ts/src/wasm"], cwd=fastdb)


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
    parser.add_argument("kind", choices=("wasm", "packages"))
    parser.add_argument("--output", type=Path)
    options = parser.parse_args()
    if options.kind == "wasm":
        wasm()
    elif options.output is None:
        parser.error("packages requires --output")
    else:
        packages(options.output.resolve())
