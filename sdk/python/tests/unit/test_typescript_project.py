"""Compile the shared Node test project beside unrelated ambient types."""

import json
from pathlib import Path
import shutil
import subprocess

import pytest

from tests.fixtures.typescript_project import write_node_tsconfig


ROOT = Path(__file__).resolve().parents[4]
TSC = ROOT / "core/foundation/c2-mem-ffi/bindings/typescript/node_modules/typescript/bin/tsc"


def test_node_project_ignores_ancestor_types(tmp_path: Path) -> None:
    node = shutil.which("node")
    if node is None or not TSC.is_file():
        pytest.skip("the pinned TypeScript compiler is not installed")

    ancestor = tmp_path / "ancestor"
    project = ancestor / "project"
    generated = project / "generated"
    generated.mkdir(parents=True)
    (generated / "main.ts").write_text("export const answer: number = projectType;\n", encoding="utf-8")
    own_types = project / "node_modules/@types/project"
    own_types.mkdir(parents=True)
    own_definition = own_types / "index.d.ts"
    own_definition.write_text("declare const projectType: number;\n", encoding="utf-8")
    ancestor_types = ancestor / "node_modules/@types/poisoned"
    ancestor_types.mkdir(parents=True)
    ancestor_definition = ancestor_types / "index.d.ts"
    ancestor_definition.write_text("declare const poisoned: MissingAncestorType;\n", encoding="utf-8")

    config_path = write_node_tsconfig(project)
    command = [node, str(TSC), "--project", str(config_path), "--noEmit"]
    compiled = subprocess.run(command, cwd=project, capture_output=True, text=True, encoding="utf-8", timeout=30, check=False)
    assert compiled.returncode == 0, compiled.stdout + compiled.stderr
    files = subprocess.run(
        [*command, "--listFilesOnly"], cwd=project, capture_output=True, text=True, encoding="utf-8", timeout=30, check=True,
    ).stdout
    listed = {Path(line).resolve() for line in files.splitlines()}
    assert own_definition.resolve() in listed
    assert ancestor_definition.resolve() not in listed

    # Prove the ancestor declaration would otherwise enter the same project.
    config = json.loads(config_path.read_text(encoding="utf-8"))
    config["compilerOptions"].pop("typeRoots", None)
    config_path.write_text(json.dumps(config), encoding="utf-8")
    polluted = subprocess.run(command, cwd=project, capture_output=True, text=True, encoding="utf-8", timeout=30, check=False)
    assert polluted.returncode != 0
    assert "MissingAncestorType" in polluted.stdout + polluted.stderr
