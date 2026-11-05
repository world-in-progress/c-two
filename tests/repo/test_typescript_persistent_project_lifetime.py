"""Keep the persistent Node fixture's project under pytest's retention policy."""

from __future__ import annotations

import ast
from pathlib import Path
from types import SimpleNamespace

import pytest


FIXTURE = Path(__file__).resolve().parents[2] / "sdk/python/tests/integration/test_typescript_persistent_ipc.py"


def test_persistent_node_project_uses_pytest_owned_directory(tmp_path: Path) -> None:
    tree = ast.parse(FIXTURE.read_text(encoding="utf-8"), filename=str(FIXTURE))
    fixture = next(
        node for node in tree.body
        if isinstance(node, ast.FunctionDef) and node.name == "persistent_node_project"
    )
    fixture.decorator_list = []
    namespace = {"shutil": SimpleNamespace(which=lambda _: None)}
    exec(compile(ast.Module(body=[fixture], type_ignores=[]), str(FIXTURE), "exec"), namespace)

    class TmpPathFactory:
        def __init__(self) -> None:
            self.paths: list[Path] = []

        def mktemp(self, basename: str) -> Path:
            path = tmp_path / f"{basename}0"
            path.mkdir()
            self.paths.append(path)
            return path

    factory = TmpPathFactory()
    with pytest.raises(AssertionError, match="real persistent IPC regression requires Node"):
        next(namespace["persistent_node_project"](factory))
    assert len(factory.paths) == 1
    assert factory.paths[0].parent == tmp_path
