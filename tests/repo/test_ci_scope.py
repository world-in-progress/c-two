from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys

import pytest
import yaml


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / ".github/scripts/ci_scope.py"


@pytest.mark.parametrize("paths,expected", [
    (["README.md", "docs/releases/0.7.6.md"], False),
    ([".github/scripts/promote_release_candidate.py", "tests/repo/test_ci_scope.py"], False),
    (["tools/ci/source_validation.py"], False),
    (["README.md", "core/transport/c2-ipc/src/pool.rs"], True),
    (["core/moved.rs", "docs/moved.rs"], True),
    (["sdk/python/pyproject.toml"], True),
    (["core/Cargo.lock"], True),
    (["uv.lock"], True),
    (["new-runtime/implementation.rs"], True),
    (["../outside"], True),
])
def test_scope_preserves_runtime_and_unknown_input_coverage(paths, expected):
    spec = importlib.util.spec_from_file_location("ci_scope", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    assert module.needs_source(paths) is expected
    assert module.needs_source(paths, truncated=True) is True


@pytest.mark.parametrize("event,force,count,expected", [
    ("pull_request", False, 1, "false"),
    ("pull_request", False, 2, "true"),
    ("push", False, 0, "false"),
    ("merge_group", False, 0, "false"),
    ("workflow_dispatch", True, 0, "true"),
    ("workflow_dispatch", False, 0, "false"),
    ("pull_request", False, 3000, "true"),
])
def test_event_selects_real_workflow_output(tmp_path, event, force, count, expected):
    output = tmp_path / "outputs"
    result = subprocess.run([sys.executable, str(SCRIPT)], env={
        **os.environ, "EVENT_NAME": event, "FORCE_FULL": str(force).lower(),
        "CHANGED_FILES": str(count), "CHANGED_FILES_JSON": json.dumps([".github/workflows/ci.yml"]),
        "GITHUB_OUTPUT": str(output),
    }, capture_output=True, text=True)
    assert result.returncode == 0, result.stderr
    assert output.read_text() == f"source={expected}\n"


def test_workflow_keeps_required_names_and_uses_full_diff():
    workflow = yaml.load((ROOT / ".github/workflows/ci.yml").read_text(), Loader=yaml.BaseLoader)
    assert {"pull_request", "push", "merge_group", "workflow_dispatch"} == set(workflow["on"])
    assert workflow["on"]["workflow_dispatch"]["inputs"]["force_full"]["default"] == "true"
    jobs = workflow["jobs"]
    changes = jobs["changes"]["steps"]
    filter_step = next(step for step in changes if step.get("id") == "filter")
    assert "github.event.merge_group.base_sha" in filter_step["with"]["base"]
    assert "github.event.before" in filter_step["with"]["base"]
    assert filter_step["with"]["list-files"] == "json"
    assert jobs["required"]["name"] == "test (py${{ matrix.python }})"
    assert jobs["required"]["if"] == "always()"
    assert "workflow-policy" in jobs["required"]["needs"]
    assert "if" not in jobs["workflow-policy"]
