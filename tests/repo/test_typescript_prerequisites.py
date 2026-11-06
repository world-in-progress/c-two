"""Missing Node builds fail before any package or interoperability work."""

import importlib.util
from pathlib import Path
import sys

import pytest


def fixture_module():
    path = Path(__file__).resolve().parents[2] / "sdk/python/tests/fixtures/typescript_project.py"
    spec = importlib.util.spec_from_file_location("typescript_project_prerequisites", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_node_types_alone_do_not_count_as_a_native_binding(tmp_path):
    fixture = fixture_module()
    package = tmp_path / "core/foundation/c2-mem-ffi/bindings/typescript"
    (package / "dist").mkdir(parents=True)
    for name in ("index.js", "index.d.ts"):
        (package / "dist" / name).touch()
    with pytest.raises(pytest.fail.Exception, match="build-runtime.mjs") as error:
        fixture.require_c2_mem_node_build(tmp_path)
    assert "c2_mem_ffi_node.node" in str(error.value)
    assert "npm ci --ignore-scripts --prefix core/" in str(error.value)
    native = package / "dist/native"
    native.mkdir()
    (native / "c2_mem_ffi_node.node").touch()
    library = {"darwin": "libc2_mem_ffi.dylib", "linux": "libc2_mem_ffi.so", "win32": "c2_mem_ffi.dll"}[sys.platform]
    with pytest.raises(pytest.fail.Exception, match=library):
        fixture.require_c2_mem_node_build(tmp_path)
    (native / library).touch()
    fixture.require_c2_mem_node_build(tmp_path)


def test_fastdb_missing_build_gives_all_preparation_commands(tmp_path):
    fixture = fixture_module()
    with pytest.raises(pytest.fail.Exception, match="fastdb4ts is not built") as error:
        fixture.require_fastdb_node_build(tmp_path)
    for command in ("npm ci --prefix", "npm run build:wasm --prefix", "npm run build --prefix"):
        assert command in str(error.value)
    for name in ("dist/index.js", "dist/payload/index.d.ts", "src/wasm/fastdb4ts.wasm"):
        path = tmp_path / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.touch()
    fixture.require_fastdb_node_build(tmp_path)
