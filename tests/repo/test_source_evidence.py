"""Source evidence is bound to the tested tree and each job's latest attempt."""
from __future__ import annotations

import copy
from pathlib import Path
import sys
from urllib.parse import parse_qs, urlsplit

import pytest

SCRIPTS = Path(__file__).resolve().parents[2] / ".github/scripts"
sys.path.insert(0, str(SCRIPTS))
import source_evidence as evidence

SOURCE_SHA = "a" * 40
HEAD_SHA = "b" * 40
TREE_SHA = "c" * 40
PREFIX = f"/repos/{evidence.CANONICAL_REPOSITORY}"


def run(run_id=201, sha=HEAD_SHA, **overrides):
    return {
        "id": run_id, "run_number": run_id, "head_sha": sha,
        "event": "pull_request", "status": "completed", "conclusion": "success",
        "repository": {"full_name": evidence.CANONICAL_REPOSITORY},
        "head_repository": {"full_name": evidence.CANONICAL_REPOSITORY},
        "path": ".github/workflows/ci.yml", "html_url": f"https://example.test/runs/{run_id}",
        **overrides,
    }


class FakeApi(evidence.GitHubApi):
    """Exercise the real API pagination/query methods against canned responses."""

    def __init__(self):
        super().__init__("https://api.test.invalid", evidence.CANONICAL_REPOSITORY, "token")
        self.paths = []
        self.pulls = [{"merged_at": "2026-10-30T00:00:00Z", "merge_commit_sha": SOURCE_SHA,
                       "head": {"sha": HEAD_SHA}}]
        self.trees = {SOURCE_SHA: TREE_SHA, HEAD_SHA: TREE_SHA}
        self.runs = [run()]
        self.jobs = {201: [{"id": i, "name": name, "run_attempt": 1,
                           "status": "completed", "conclusion": "success"}
                          for i, name in enumerate(evidence.REQUIRED_JOBS)]}
        self.artifacts = {201: [self.artifact(label) for label in evidence.FULL_LANES]}

    @staticmethod
    def artifact(label, attempt=1, sha=HEAD_SHA):
        return {"id": evidence.FULL_LANES.index(label) + attempt * 100,
                "name": f"source-{label}-py3.12-full-{sha}-a{attempt}", "expired": False}

    def get_json(self, path):
        self.paths.append(path)
        parsed = urlsplit(path)
        query = parse_qs(parsed.query)
        if parsed.path == f"{PREFIX}/commits/{SOURCE_SHA}/pulls":
            return 200, copy.deepcopy(self.pulls)
        if parsed.path.startswith(f"{PREFIX}/git/commits/"):
            sha = parsed.path.rsplit("/", 1)[1]
            return 200, {"tree": {"sha": self.trees[sha]}}
        if parsed.path == f"{PREFIX}/actions/workflows/ci.yml/runs":
            assert set(query) == {"head_sha", "per_page", "page"}
            return 200, {"workflow_runs": [copy.deepcopy(r) for r in self.runs
                                           if r["head_sha"] == query["head_sha"][0]]}
        for run_id in self.jobs:
            if parsed.path == f"{PREFIX}/actions/runs/{run_id}/jobs":
                assert query["filter"] == ["all"]
                return 200, {"jobs": copy.deepcopy(self.jobs[run_id])}
            if parsed.path == f"{PREFIX}/actions/runs/{run_id}/artifacts":
                return 200, {"artifacts": copy.deepcopy(self.artifacts[run_id])}
        raise AssertionError(f"Unexpected API request: {path}")

    def get_bytes(self, *args, **kwargs):
        raise AssertionError("Source evidence must not download artifact bytes")


@pytest.mark.parametrize("fork", [False, True])
def test_same_tree_pr_run_qualifies_including_fork_head(fork):
    api = FakeApi()
    if fork:
        api.runs[0]["head_repository"] = {"full_name": "contributor/c-two"}
    selected = evidence.find_source_evidence(api, SOURCE_SHA)
    assert selected == {
        "run_id": 201, "run_url": "https://example.test/runs/201", "event": "pull_request",
        "tested_sha": HEAD_SHA, "tree_sha": TREE_SHA,
        "job_attempts": dict.fromkeys(evidence.REQUIRED_JOBS, 1),
        "artifacts": {label: {"id": api.artifact(label)["id"], "name": api.artifact(label)["name"],
                               "run_attempt": 1} for label in evidence.FULL_LANES},
    }
    assert len([p for p in api.paths if "/git/commits/" in p]) == 2


@pytest.mark.parametrize("event", ["push", "workflow_dispatch", "pull_request"])
def test_release_commit_itself_can_supply_source_evidence(event):
    api = FakeApi()
    api.runs = [run(sha=SOURCE_SHA, event=event)]
    api.artifacts[201] = [api.artifact(label, sha=SOURCE_SHA) for label in evidence.FULL_LANES]
    assert evidence.find_source_evidence(api, SOURCE_SHA)["tested_sha"] == SOURCE_SHA


def test_different_tree_pr_run_is_rejected_with_reason():
    api = FakeApi()
    api.trees[HEAD_SHA] = "d" * 40
    with pytest.raises(evidence.PromotionError, match="run 201: .*different tree"):
        evidence.find_source_evidence(api, SOURCE_SHA)
    assert not any("/jobs?" in path for path in api.paths)


@pytest.mark.parametrize("pr_change", [{"merged_at": None}, {"merge_commit_sha": "d" * 40}])
def test_only_merged_pr_for_the_release_commit_supplies_a_candidate(pr_change):
    api = FakeApi()
    api.pulls[0].update(pr_change)
    with pytest.raises(evidence.PromotionError, match="no runs found"):
        evidence.find_source_evidence(api, SOURCE_SHA)
    assert not any(f"head_sha={HEAD_SHA}" in path for path in api.paths)


@pytest.mark.parametrize("override,reason", [
    ({"repository": {"full_name": "fork/c-two"}}, "repository mismatch"),
    ({"path": ".github/workflows/other.yml"}, "workflow path mismatch"),
    ({"event": "merge_group"}, "unsupported event"),
    ({"status": "in_progress", "conclusion": None}, "in_progress/None"),
    ({"conclusion": "failure"}, "completed/failure"),
])
def test_run_provenance_and_terminal_success_are_required(override, reason):
    api = FakeApi()
    api.runs[0].update(override)
    with pytest.raises(evidence.PromotionError, match=reason):
        evidence.find_source_evidence(api, SOURCE_SHA)


@pytest.mark.parametrize("name", evidence.REQUIRED_JOBS)
@pytest.mark.parametrize("status,conclusion", [("completed", "failure"), ("in_progress", None),
                                               ("completed", "skipped")])
def test_latest_required_attempt_cannot_use_older_success(name, status, conclusion):
    api = FakeApi()
    api.jobs[201].append({"id": 100, "name": name, "run_attempt": 2,
                          "status": status, "conclusion": conclusion})
    with pytest.raises(evidence.PromotionError, match="latest attempt 2"):
        evidence.find_source_evidence(api, SOURCE_SHA)


@pytest.mark.parametrize("label", evidence.FULL_LANES)
def test_rerun_selects_matching_latest_attempt_and_keeps_untouched_lanes(label):
    api = FakeApi()
    job = next(j for j in api.jobs[201] if j["name"] == f"source ({label}, py3.12)")
    job["conclusion"] = "failure"
    api.jobs[201].insert(0, {**job, "id": 100, "run_attempt": 3, "conclusion": "success"})
    # A stale failed attempt may remain next to the successful latest attempt.
    api.jobs[201].append({**job, "id": 101, "run_attempt": 2})
    api.artifacts[201].extend([api.artifact(label, 2), api.artifact(label, 3)])
    selected = evidence.find_source_evidence(api, SOURCE_SHA)
    assert selected["job_attempts"][job["name"]] == 3
    for lane in evidence.FULL_LANES:
        expected = 3 if lane == label else 1
        assert selected["artifacts"][lane]["run_attempt"] == expected
        assert selected["artifacts"][lane]["name"].endswith(f"-a{expected}")


@pytest.mark.parametrize("label", evidence.FULL_LANES)
@pytest.mark.parametrize("problem", ["missing", "old-attempt-only", "duplicate", "expired"])
def test_latest_full_lane_requires_one_unexpired_matching_artifact(label, problem):
    api = FakeApi()
    artifact = next(a for a in api.artifacts[201] if a["name"] == api.artifact(label)["name"])
    if problem == "missing":
        api.artifacts[201].remove(artifact)
    elif problem == "old-attempt-only":
        job = next(j for j in api.jobs[201] if j["name"] == f"source ({label}, py3.12)")
        job["run_attempt"] = 2
    elif problem == "duplicate":
        api.artifacts[201].append(dict(artifact, id=999))
    else:
        artifact["expired"] = True
    with pytest.raises(evidence.PromotionError, match=f"source-{label}-py3.12-full-"):
        evidence.find_source_evidence(api, SOURCE_SHA)


@pytest.mark.parametrize("name", evidence.REQUIRED_JOBS)
def test_policy_only_or_incomplete_job_inventory_cannot_qualify(name):
    api = FakeApi()
    api.jobs[201] = [j for j in api.jobs[201] if j["name"] != name]
    with pytest.raises(evidence.PromotionError, match="required job .* is missing"):
        evidence.find_source_evidence(api, SOURCE_SHA)


def test_selects_newest_qualified_run_and_records_rejections_if_none_qualify():
    api = FakeApi()
    api.runs.extend([run(202), run(203, conclusion="failure")])
    api.jobs[202] = copy.deepcopy(api.jobs[201])
    api.artifacts[202] = copy.deepcopy(api.artifacts[201])
    assert evidence.find_source_evidence(api, SOURCE_SHA)["run_id"] == 202
    api.jobs[202] = []
    api.artifacts[201] = []
    with pytest.raises(evidence.PromotionError) as error:
        evidence.find_source_evidence(api, SOURCE_SHA)
    assert all(f"run {run_id}:" in str(error.value) for run_id in [201, 202, 203])


def test_other_commit_search_cannot_reuse_release_commits_own_run():
    api = FakeApi()
    api.runs.append(run(202, sha=SOURCE_SHA))
    selected = evidence.find_source_evidence(api, SOURCE_SHA, other_commit=True)
    assert selected["run_id"] == 201 and selected["tested_sha"] == HEAD_SHA
    assert not any(f"head_sha={SOURCE_SHA}" in p for p in api.paths)


def test_git_api_failure_propagates_without_accepting_another_run():
    api = FakeApi()
    original = api.get_json

    def get_json(path):
        if path == f"{PREFIX}/git/commits/{HEAD_SHA}":
            raise evidence.PromotionError("GitHub API failed")
        return original(path)

    api.get_json = get_json
    with pytest.raises(evidence.PromotionError, match="GitHub API failed"):
        evidence.find_source_evidence(api, SOURCE_SHA)


@pytest.mark.parametrize("job_filter", [False, True])
def test_job_queries_preserve_candidate_latest_filter_and_expose_source_attempts(job_filter):
    api = evidence.GitHubApi("https://api.test.invalid", evidence.CANONICAL_REPOSITORY, "token")
    paths = []

    def get_json(path):
        paths.append(path)
        return 200, {"jobs": []}

    api.get_json = get_json
    api.run_jobs(201, all_attempts=job_filter)
    assert f"filter={'all' if job_filter else 'latest'}" in paths[0]


def test_list_pagination_supports_pull_lists_and_workflow_objects():
    api = evidence.GitHubApi("https://api.test.invalid", evidence.CANONICAL_REPOSITORY, "token")
    for key in [None, "workflow_runs"]:
        paths = []

        def get_json(path):
            paths.append(path)
            entries = [{"id": i} for i in range(100)] if len(paths) == 1 else [{"id": 100}]
            return 200, entries if key is None else {key: entries}

        api.get_json = get_json
        assert len(api.list_all("/list", key)) == 101
        assert paths == ["/list?per_page=100&page=1", "/list?per_page=100&page=2"]
