"""Read-only GitHub access and tree-bound Source Validation evidence selection."""
from __future__ import annotations

import json
import urllib.error
import urllib.parse
import urllib.request
from typing import Any

CANONICAL_REPOSITORY = "world-in-progress/c-two"
FULL_LANES = ("ubuntu-24.04", "macos-15", "windows-2025", "windows-11-arm")
REQUIRED_JOBS = (
    *(f"source ({label}, py3.12)" for label in FULL_LANES),
    "source (ubuntu-24.04, py3.14t)",
    "workflow policy",
)


class PromotionError(RuntimeError):
    """A fail-closed condition: the candidate must not be promoted."""


def _url_origin(url: str) -> tuple[str, str | None, int | None]:
    try:
        parsed = urllib.parse.urlsplit(url)
        port = parsed.port
    except ValueError as error:
        raise PromotionError("GitHub API redirect has an invalid origin") from error
    if port is None:
        port = {"http": 80, "https": 443}.get(parsed.scheme)
    return parsed.scheme, parsed.hostname, port


class _GitHubApiRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Keep API auth on its origin and use signed artifact URLs without it."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        source_origin = _url_origin(req.full_url)
        target_origin = _url_origin(newurl)
        cross_origin = source_origin != target_origin
        if cross_origin and (source_origin[0] != "https" or target_origin[0] != "https"):
            raise PromotionError("GitHub API cross-origin redirect requires HTTPS")
        redirected = super().redirect_request(req, fp, code, msg, headers, newurl)
        if cross_origin and redirected is not None:
            redirected.remove_header("Authorization")
        return redirected


class GitHubApi:
    """Read-only authenticated access to the canonical repository's Actions API."""

    def __init__(self, base_url: str, repository: str, token: str) -> None:
        self.base_url = base_url.rstrip("/")
        self.repository = repository
        self.token = token
        self._opener = urllib.request.build_opener(_GitHubApiRedirectHandler())

    def _request(self, path: str, accept: str) -> tuple[int, bytes]:
        if not path.startswith("/"):
            raise PromotionError(f"internal: API path must be absolute: {path}")
        request = urllib.request.Request(
            f"{self.base_url}{path}",
            headers={
                "Accept": accept,
                "Authorization": f"Bearer {self.token}",
                "X-GitHub-Api-Version": "2022-11-28",
                "User-Agent": "c-two-release-promotion",
            },
            method="GET",
        )
        try:
            with self._opener.open(request, timeout=120) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as error:
            return error.code, error.read()
        except (urllib.error.URLError, OSError) as error:
            raise PromotionError(f"GitHub API request failed for {path}: {error}") from error

    def get_json(self, path: str) -> tuple[int, Any]:
        status, body = self._request(path, "application/vnd.github+json")
        if status not in (200, 404):
            raise PromotionError(f"GitHub API {path} returned HTTP {status}: {body[:400]!r}")
        if status == 404:
            return status, None
        try:
            return status, json.loads(body.decode("utf-8"))
        except (UnicodeError, json.JSONDecodeError) as error:
            raise PromotionError(f"GitHub API {path} returned invalid JSON: {error}") from error

    def get_bytes(self, path: str, accept: str = "application/vnd.github+json") -> tuple[int, bytes]:
        return self._request(path, accept)

    def list_all(self, path: str, key: str | None) -> list[dict[str, Any]]:
        items: list[dict[str, Any]] = []
        page = 1
        while True:
            separator = "&" if "?" in path else "?"
            status, payload = self.get_json(f"{path}{separator}per_page=100&page={page}")
            if status == 404:
                raise PromotionError(f"GitHub API listing not found: {path}")
            batch = payload if key is None else payload.get(key) if isinstance(payload, dict) else None
            if not isinstance(batch, list):
                raise PromotionError(f"GitHub API listing {path} returned no {key} list")
            items.extend(entry for entry in batch if isinstance(entry, dict))
            if len(batch) < 100:
                return items
            page += 1

    def workflow_runs(self, workflow_file: str, *, head_sha: str) -> list[dict[str, Any]]:
        return self.list_all(
            f"/repos/{self.repository}/actions/workflows/{workflow_file}/runs?head_sha={head_sha}",
            "workflow_runs",
        )

    def run_jobs(self, run_id: int, *, all_attempts: bool = False) -> list[dict[str, Any]]:
        job_filter = "all" if all_attempts else "latest"
        return self.list_all(
            f"/repos/{self.repository}/actions/runs/{run_id}/jobs?filter={job_filter}", "jobs")

    def run_artifacts(self, run_id: int) -> list[dict[str, Any]]:
        return self.list_all(f"/repos/{self.repository}/actions/runs/{run_id}/artifacts", "artifacts")


def _require(condition: object, message: str) -> None:
    if not condition:
        raise PromotionError(message)


def _select_jobs(jobs: list[dict[str, Any]]) -> dict[str, int]:
    attempts = {}
    for name in REQUIRED_JOBS:
        matches = [job for job in jobs if job.get("name") == name]
        _require(matches, f"required job {name!r} is missing")
        latest = max(matches, key=lambda job: job["run_attempt"])
        _require(latest.get("status") == "completed" and latest.get("conclusion") == "success",
                 f"required job {name!r} latest attempt {latest['run_attempt']} "
                 f"is {latest.get('status')}/{latest.get('conclusion')}")
        attempts[name] = latest["run_attempt"]
    return attempts


def _select_artifacts(
    artifacts: list[dict[str, Any]], tested_sha: str, attempts: dict[str, int],
) -> dict[str, dict[str, Any]]:
    selected = {}
    for label in FULL_LANES:
        attempt = attempts[f"source ({label}, py3.12)"]
        name = f"source-{label}-py3.12-full-{tested_sha}-a{attempt}"
        matches = [artifact for artifact in artifacts if artifact.get("name") == name]
        _require(len(matches) == 1, f"expected exactly one {name} artifact, found {len(matches)}")
        artifact = matches[0]
        _require(not artifact["expired"], f"artifact {name} has expired")
        selected[label] = {"id": artifact["id"], "name": name, "run_attempt": attempt}
    return selected


def find_source_evidence(
    api: GitHubApi, source_sha: str, *, other_commit: bool = False,
) -> dict[str, Any]:
    """Select the newest qualifying run for the release tree, without downloading bytes.

    Associated merged PR heads are the only additional tested commits. Scope
    selection can restrict the search to those heads to reuse pre-merge proof.
    API failures propagate; rejected runs are reported when none qualifies.
    """
    _require(api.repository == CANONICAL_REPOSITORY, "source evidence requires the canonical repository")
    prefix = f"/repos/{api.repository}"
    trees: dict[str, str] = {}

    def tree(sha: str) -> str:
        if sha not in trees:
            status, commit = api.get_json(f"{prefix}/git/commits/{sha}")
            _require(status == 200, f"Git commit {sha} not found")
            trees[sha] = commit["tree"]["sha"]
        return trees[sha]

    release_tree = tree(source_sha)
    pulls = api.list_all(f"{prefix}/commits/{source_sha}/pulls", None)
    candidates = {source_sha}
    candidates.update(pr["head"]["sha"] for pr in pulls
                      if pr.get("merged_at") and pr.get("merge_commit_sha") == source_sha)
    if other_commit:
        candidates.discard(source_sha)
    runs = {run["id"]: run for sha in sorted(candidates)
            for run in api.workflow_runs("ci.yml", head_sha=sha)}
    reasons = []
    for run in sorted(runs.values(), key=lambda run: (run.get("run_number", 0), run["id"]), reverse=True):
        run_id = run["id"]
        try:
            _require(run.get("repository", {}).get("full_name") == CANONICAL_REPOSITORY,
                     "repository mismatch")
            _require(run.get("path", "").split("@", 1)[0] == ".github/workflows/ci.yml",
                     "workflow path mismatch")
            _require(run.get("event") in {"push", "workflow_dispatch", "pull_request"},
                     f"unsupported event {run.get('event')!r}")
            _require(run.get("status") == "completed" and run.get("conclusion") == "success",
                     f"run is {run.get('status')}/{run.get('conclusion')}")
        except PromotionError as error:
            reasons.append(f"run {run_id}: {error}")
            continue
        tested_sha = run["head_sha"]
        if tree(tested_sha) != release_tree:
            reasons.append(f"run {run_id}: tested commit {tested_sha} has a different tree")
            continue
        jobs = api.run_jobs(run_id, all_attempts=True)
        artifacts = api.run_artifacts(run_id)
        try:
            attempts = _select_jobs(jobs)
            selected = _select_artifacts(artifacts, tested_sha, attempts)
        except PromotionError as error:
            reasons.append(f"run {run_id}: {error}")
            continue
        return {"run_id": run_id, "run_url": run["html_url"], "event": run["event"],
                "tested_sha": tested_sha, "tree_sha": release_tree,
                "job_attempts": attempts, "artifacts": selected}
    detail = "; ".join(reasons) if reasons else f"no runs found for tested commits {sorted(candidates)}"
    raise PromotionError(f"no qualifying Source Validation for release {source_sha}: {detail}")
