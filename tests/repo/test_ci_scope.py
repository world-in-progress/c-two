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
        **os.environ, "GITHUB_REPOSITORY": "", "GITHUB_TOKEN": "", "GITHUB_SHA": "a" * 40,
        "EVENT_NAME": event, "FORCE_FULL": str(force).lower(),
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
    assert set(jobs["source"]["needs"]) == {"changes"}
    assert "if" not in jobs["workflow-policy"]


@pytest.mark.parametrize("paths,runtime", [(["docs/releases/0.10.1.md"], False), (["core/runtime.rs"], True)])
@pytest.mark.parametrize("lookup", ["same-tree-pr", "fork-pr", "different-tree", "no-run", "api-error"])
def test_push_keeps_path_scope_and_reuses_qualified_pr_evidence(tmp_path, monkeypatch, capsys, paths, runtime, lookup):
    from test_source_evidence import FakeApi, SOURCE_SHA

    spec = importlib.util.spec_from_file_location("ci_scope", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    api = FakeApi()
    if lookup == "fork-pr":
        api.runs[0]["head_repository"] = {"full_name": "contributor/c-two"}
    elif lookup == "different-tree":
        api.trees[api.runs[0]["head_sha"]] = "d" * 40
    elif lookup == "no-run":
        api.runs = []
    elif lookup == "api-error":
        def failed_request(path):
            raise module.PromotionError("GitHub API unavailable")
        api.get_json = failed_request
    monkeypatch.setattr(module, "GitHubApi", lambda *args: api)
    output = tmp_path / "outputs"
    for key, value in {
        "EVENT_NAME": "push", "FORCE_FULL": "false", "GITHUB_SHA": SOURCE_SHA,
        "GITHUB_TOKEN": "token", "GITHUB_REPOSITORY": api.repository,
        "CHANGED_FILES_JSON": json.dumps(paths), "CHANGED_FILES": "1",
        "GITHUB_OUTPUT": str(output),
    }.items():
        monkeypatch.setenv(key, value)
    module.main()
    reuse = runtime and lookup in {"same-tree-pr", "fork-pr"}
    assert output.read_text() == f"source={str(runtime and not reuse).lower()}\n"
    console = capsys.readouterr().out
    if not runtime:
        assert api.paths == []
        assert "Workflow policy validation" in console
    elif reuse:
        assert "201" in console and api.runs[0]["html_url"] in console
    else:
        assert "requiring full validation" in console


def test_explicit_full_validation_does_not_lookup_previous_evidence(tmp_path, monkeypatch):
    spec = importlib.util.spec_from_file_location("ci_scope", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)

    def unexpected_lookup(*args, **kwargs):
        raise AssertionError("forced source validation cannot be omitted")

    monkeypatch.setattr(module, "GitHubApi", unexpected_lookup)
    output = tmp_path / "outputs"
    monkeypatch.setenv("EVENT_NAME", "push")
    monkeypatch.setenv("FORCE_FULL", "true")
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    module.main()
    assert output.read_text() == "source=true\n"


def test_push_outside_the_canonical_repository_runs_policy_only(tmp_path, monkeypatch, capsys):
    spec = importlib.util.spec_from_file_location("ci_scope", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)

    def unexpected_lookup(*args, **kwargs):
        raise AssertionError("a fork push mirrors canonical main and needs no evidence lookup")

    monkeypatch.setattr(module, "GitHubApi", unexpected_lookup)
    output = tmp_path / "outputs"
    for key, value in {
        "EVENT_NAME": "push", "FORCE_FULL": "false", "GITHUB_SHA": "a" * 40,
        "GITHUB_TOKEN": "token", "GITHUB_REPOSITORY": "contributor/c-two",
        "CHANGED_FILES_JSON": '["core/runtime.rs"]', "CHANGED_FILES": "1",
        "GITHUB_OUTPUT": str(output),
    }.items():
        monkeypatch.setenv(key, value)
    module.main()
    assert output.read_text() == "source=false\n"
    assert "outside world-in-progress/c-two" in capsys.readouterr().out


def test_scope_lookup_permissions_and_attempt_names_stay_on_the_producer():
    workflow = yaml.load((ROOT / ".github/workflows/ci.yml").read_text(), Loader=yaml.BaseLoader)
    jobs = workflow["jobs"]
    assert "actions" not in workflow["permissions"]
    assert jobs["changes"]["permissions"]["actions"] == "read"
    select = next(step for step in jobs["changes"]["steps"] if step.get("id") == "select")
    assert select["env"]["GITHUB_TOKEN"] == "${{ github.token }}"
    for name, job in jobs.items():
        if name != "changes":
            assert "actions" not in job.get("permissions", {})
    upload = next(step for step in jobs["source"]["steps"] if step.get("uses") == "actions/upload-artifact@v4")
    assert upload["with"]["name"].endswith("-a${{ github.run_attempt }}")
    assert set(jobs["required"]["needs"]) == {"changes", "workflow-policy", "source"}


@pytest.mark.parametrize("missing", ["GITHUB_TOKEN", "GITHUB_REPOSITORY", "GITHUB_SHA"])
def test_runtime_push_missing_workflow_configuration_fails(tmp_path, monkeypatch, missing):
    from test_source_evidence import FakeApi, SOURCE_SHA

    spec = importlib.util.spec_from_file_location("ci_scope", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    api = FakeApi()
    monkeypatch.setattr(module, "GitHubApi", lambda *args: api)
    output = tmp_path / "outputs"
    for key, value in {
        "EVENT_NAME": "push", "FORCE_FULL": "false", "GITHUB_SHA": SOURCE_SHA,
        "GITHUB_TOKEN": "token", "GITHUB_REPOSITORY": api.repository,
        "CHANGED_FILES_JSON": '["core/runtime.rs"]', "CHANGED_FILES": "1",
        "GITHUB_OUTPUT": str(output),
    }.items():
        monkeypatch.setenv(key, value)
    monkeypatch.delenv(missing)
    with pytest.raises(KeyError, match=missing):
        module.main()
    assert not output.exists() and api.paths == []
