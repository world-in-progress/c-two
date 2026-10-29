"""Prepare an exact-tested-candidate promotion without ever publishing.

This is step 2 of the release plan: the ``Release Candidate`` workflow — an
explicit ``workflow_dispatch`` on canonical ``main`` only — builds and
validates byte-exact artifacts bound to one source commit, and this script
promotes those retained bytes — it never rebuilds, never executes candidate
code, and never mutates remote state. Publication (GitHub release assets,
PyPI upload) is performed by the calling workflow only after this script
returns exit code 0 with a plan.

Contract, fail-closed on every deviation:

- Promotion is explicit only. ``prepare`` requires ``--candidate-run-id``
  naming one successful ``workflow_dispatch`` ``Release Candidate`` run on
  ``refs/heads/main`` of the canonical repository; push-era candidates,
  PR/fork/merge-group runs, and version scanning are all rejected. There is
  no automatic ``workflow_run`` listener, no deferred exit code, and no
  already-published skip: a gate that has not completed successfully is an
  explicit incomplete-gate failure, never a deferred or faked success.
- The candidate's ``rc-manifest.json`` (schema ``c-two.release-candidate.v1``,
  sidecar-verified by ``release_candidate.py``) and ``rc-context.json`` must
  record ``event: workflow_dispatch`` on ``refs/heads/main``, and the context
  must bind exactly one successful ``Source Validation`` run through
  ``source_validation_run_id`` (selected by ``check-source``). That bound run
  — a canonical ``main`` push or explicit dispatch for the exact same source
  SHA — is the only accepted source of the five full-scope platform artifacts
  (``ubuntu-24.04``, ``macos-15``, ``windows-2022``, ``windows-2025``, ``windows-11-arm``) and
  their receipts. Evidence is never mixed across runs, and a different
  Source Validation run for the same source cannot substitute the bound one.
- ``check-source --source-sha`` is the read-only companion the Release
  Candidate workflow uses to fill ``source_validation_run_id``: it reads only
  GitHub metadata (workflow-run, job, and artifact listings, never artifact
  downloads) and returns the newest successful canonical-main Source
  Validation run whose complete job inventory carries the five platform
  ``py3.12`` full source jobs plus the Ubuntu ``py3.14t`` source job — a
  policy-only green run is not a full gate — and exactly one full-scope
  artifact per platform, as JSON ``{"source_sha", "run_id", "run_url"}``.
- The promoting run itself must be on ``refs/heads/main`` of the canonical
  repository; the trusted validation code is the current ``main`` checkout,
  never the candidate SHA.
- Candidate ``rc-*`` ZIPs match the producer's collection scope; compiler-cache
  diagnostics are not release inputs. Each selected ZIP is downloaded through
  the authenticated GitHub Actions API, its SHA-256 is checked against the
  API-reported ``digest``, and it is
  extracted only through the reviewed safe-name/safe-member validators in
  ``tools/local_rc/artifact_manifest.py``. A repeated member name across
  artifact ZIPs is accepted only when its SHA-256 is identical.
- The candidate's ``rc-manifest.json`` (schema ``c-two.release-candidate.v1``,
  sidecar-verified by ``release_candidate.py``) is re-hashed entry by entry
  against the extracted bytes: 6 exact CLI names, 35 unique platform/ABI
  wheel rows, 1 sdist, canonical installer assets, FastDB proof inputs,
  versions/source identity, and the executed receipts that bind the tested
  bytes. The receipt reader mirrors the reviewed candidate contract:
  ``c-two.installed-wheel-smoke.v1`` ABI rows with ``source_commit_sha``,
  ``row``, ``wheel_abi`` (interpreter ``cp314`` / ABI ``cp314t`` pairs for
  free-threaded rows), FastDB decisions cross-bound against the verified
  published listing and the single retained sdist, plus lifecycle cleanup
  (processes exited, temporary directory removed, borrowed inputs
  invalidated).
- The Source Validation full-scope evidence is re-validated with the trusted
  runner source: the full gate inventory from ``tools/ci/source_validation.py``
  must be present and passing with no skipped gates, every internal artifact
  hash in ``run-evidence.json`` is verified against the extracted bytes, the
  strict 18-row Rust/Python portable matrix and 12-row TypeScript receipts
  are re-validated through ``tools/local_rc``, the ordinary wheel receipts
  and Windows standard-user receipts bind each source lane's debug ``c3`` (a different build from the
  released candidate executables, never claimed byte-equal), and both source
  pins (C-Two commit and the frozen FastDB revision) must match exactly.
- FastDB dependency-proof inputs are never classified or staged as C-Two
  packages; the PyPI staging directory holds exactly the registry-missing
  files after byte-exact remote resolution.
- Idempotent reruns skip only remote assets/registry files whose bytes
  already match. Differing bytes, versions, tag sources, or foreign assets
  fail; tags are never moved and assets are never clobbered. ``--dry-run``
  performs the identical read-only verification, including all remote
  comparisons, and never publishes.

Exit codes: ``0`` plan ready, ``1`` validation failure with diagnostics on
stderr.
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import stat
import sys
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from collections.abc import Iterable
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPTS_DIR = REPO_ROOT / ".github" / "scripts"
for _path in (str(REPO_ROOT), str(SCRIPTS_DIR)):
    if _path not in sys.path:
        sys.path.insert(0, _path)

from tools.local_rc.artifact_manifest import (  # noqa: E402
    CandidateError,
    checked_relative_path,
    sha256_bytes,
    sha256_file,
    zip_inventory,
)
from tools.local_rc import (  # noqa: E402
    portable_matrix_receipt,
    typescript_receipt,
)
import release_candidate as rc_helper  # noqa: E402
from packaging.utils import InvalidWheelFilename, parse_wheel_filename  # noqa: E402

EXPECT_REPOSITORY = "world-in-progress/c-two"
EXPECT_REF = "refs/heads/main"
CANDIDATE_WORKFLOW_FILE = "release-candidate.yml"
CANDIDATE_WORKFLOW_NAME = "Release Candidate"
SOURCE_VALIDATION_WORKFLOW_FILE = "ci.yml"
SOURCE_VALIDATION_WORKFLOW_NAME = "Source Validation"
# Candidates exist only as explicit canonical-main dispatches; the source
# gate additionally still accepts canonical main pushes.
CANDIDATE_EVENT = "workflow_dispatch"
SOURCE_GATE_EVENTS = ("push", "workflow_dispatch")
MANIFEST_SCHEMA = "c-two.release-candidate.v1"
MANIFEST_NAME = "rc-manifest.json"
CONTEXT_SCHEMA = "c-two.release-candidate.context.v1"
PLAN_SCHEMA = "c-two.release-promotion-plan.v1"
PYPI_HOST = "https://pypi.org"

CLI_TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
)
WINDOWS_TARGETS = ("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc")
EXPECTED_CLI_NAMES = tuple(
    f"c3-{target}{'.exe' if target in WINDOWS_TARGETS else ''}" for target in CLI_TARGETS
)
EXPECTED_PYTHONS = ("3.10", "3.11", "3.12", "3.13", "3.14", "3.14t")
# Advertised row -> exact tag pair. A free-threaded 3.14t wheel is
# interpreter cp314 with ABI cp314t (c_two-...-cp314-cp314t-<plat>.whl); it
# must never be confused with the GIL build's cp314-cp314 pair.
PYTHON_ABI_PAIRS = {
    "3.10": ("cp310", "cp310"),
    "3.11": ("cp311", "cp311"),
    "3.12": ("cp312", "cp312"),
    "3.13": ("cp313", "cp313"),
    "3.14": ("cp314", "cp314"),
    "3.14t": ("cp314", "cp314t"),
}
EXPECTED_ABI_ROWS = {(target, python) for target in CLI_TARGETS for python in EXPECTED_PYTHONS
                     if rc_helper.target_supports_python(target, python)}
EXPECTED_RECEIPT_PREFIX_COUNTS = {
    "rc-context": 1,
    "rc-fastdb-public": 1,
    "fastdb-": 7,
    "smoke-": 6,
    "abi-": 35,
    "standard-user-": 4,
    "sdist-": 2,
}
# Installers are retained by the candidate under their canonical release
# asset names; the promotion publishes those exact bytes under the same names.
EXPECTED_INSTALLER_NAMES = ("c3-installer.sh", "c3-installer.ps1")
FASTDB_LICENSE_ASSETS = ("fastdb-LICENSE", "fastdb-THIRD_PARTY_NOTICES.txt")
SOURCE_VALIDATION_LANES = {
    "ubuntu-24.04": "linux",
    "macos-15": "macos",
    "windows-2022": "windows",
    "windows-2025": "windows",
    "windows-11-arm": "windows",
}
# The complete source-job inventory check-source demands from one successful
# run: the five platform py3.12 full lanes plus the Ubuntu py3.14t lane.
CHECK_SOURCE_REQUIRED_JOBS = (
    *(f"source ({label}, py3.12)" for label in SOURCE_VALIDATION_LANES),
    "source (ubuntu-24.04, py3.14t)",
    "workflow policy",
)
# Frozen FastDB source pin of the Source Validation gates; cross-checked against
# the trusted checkout's ci.yml so silent pin drift fails closed.
SOURCE_VALIDATION_FASTDB_CORE_VERSION = "0.2.3"
SOURCE_VALIDATION_FASTDB_SHA = "abba785ef2a7d3fbee1bbc608393a3e6c7b042c4"
SMOKE_SCHEMA = "c-two.installed-wheel-smoke.v1"
STANDARD_USER_WRAPPER_SCHEMA = "c-two.standard-user-wrapper.v1"
SDIST_INSPECT_SCHEMA = "c-two.release-candidate.sdist-inspection.v1"
SDIST_PROOF_SCHEMA = "c-two.release-candidate.sdist-proof.v1"
SOURCE_EVIDENCE_SCHEMA = "c-two.source-run-evidence.v1"
MATRIX_RECEIPT_NAME = "python-suite/portable.v1.json"
TYPESCRIPT_RECEIPT_NAME = "python-suite/typescript.v1.json"
SOURCE_WHEEL_RECEIPT = "installed-wheel-full-receipt.v1.json"
SOURCE_STANDARD_USER_RECEIPT = "installed-wheel-standard-user-full-receipt.v1.json"
SOURCE_STANDARD_USER_WRAPPER = SOURCE_STANDARD_USER_RECEIPT + ".wrapper.json"
PYTHON_RUN_RECEIPT_NAME = "python-suite/run.json"
TAG_PREFIX = "c3-v"
SHA_PATTERN = re.compile(r"^[0-9a-f]{40}$")


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

    def list_all(self, path: str, key: str) -> list[dict[str, Any]]:
        items: list[dict[str, Any]] = []
        page = 1
        while True:
            separator = "&" if "?" in path else "?"
            status, payload = self.get_json(f"{path}{separator}per_page=100&page={page}")
            if status == 404:
                raise PromotionError(f"GitHub API listing not found: {path}")
            batch = payload.get(key) if isinstance(payload, dict) else None
            if not isinstance(batch, list):
                raise PromotionError(f"GitHub API listing {path} returned no {key} list")
            items.extend(entry for entry in batch if isinstance(entry, dict))
            if len(batch) < 100:
                return items
            page += 1

    def workflow_runs(self, workflow_file: str, *, event: str, branch: str, head_sha: str) -> list[dict[str, Any]]:
        return self.list_all(
            f"/repos/{self.repository}/actions/workflows/{workflow_file}/runs"
            f"?event={event}&branch={branch}&head_sha={head_sha}",
            "workflow_runs",
        )

    def run_jobs(self, run_id: int) -> list[dict[str, Any]]:
        return self.list_all(f"/repos/{self.repository}/actions/runs/{run_id}/jobs?filter=latest", "jobs")

    def run_artifacts(self, run_id: int) -> list[dict[str, Any]]:
        return self.list_all(f"/repos/{self.repository}/actions/runs/{run_id}/artifacts", "artifacts")


def _sidecar_name(name: str) -> str:
    return f"{name}.sha256"


def _load_json_file(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8-sig"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise PromotionError(f"invalid JSON {path.name}: {error}") from error


def _require(condition: object, message: str) -> None:
    if not condition:
        raise PromotionError(message)


def validate_gate_run(
    api: GitHubApi, run: dict[str, Any], workflow_file: str, workflow_name: str,
    source_sha: str, allowed_events: tuple[str, ...],
) -> None:
    """Fail closed unless this run is a canonical-main gate run for the exact SHA."""
    run_id = run.get("id")
    _require(run.get("name") == workflow_name,
             f"{workflow_name} run {run_id} has unexpected name {run.get('name')!r}")
    _require(run.get("event") in allowed_events,
             f"{workflow_name} run {run_id} event {run.get('event')!r} is not one of "
             f"{list(allowed_events)}")
    _require(run.get("head_branch") == "main" and run.get("head_sha") == source_sha,
             f"{workflow_name} run {run_id} provenance mismatch")
    _require(str(run.get("path", "")).split("@", 1)[0] == f".github/workflows/{workflow_file}",
             f"{workflow_name} run {run_id} workflow path mismatch")
    _require(run.get("repository", {}).get("full_name") == api.repository
             and run.get("head_repository", {}).get("full_name") == api.repository,
             f"{workflow_name} run {run_id} repository mismatch")


def require_gate_success(workflow_name: str, run: dict[str, Any]) -> None:
    """A gate must have completed successfully; a pending gate is an explicit failure."""
    run_id = run.get("id")
    status = run.get("status")
    _require(status == "completed",
             f"{workflow_name} run {run_id} is still {status!r}; the gate is incomplete — "
             "promotion never reports success while a gate is pending; rerun it after the "
             "run finishes")
    _require(run.get("conclusion") == "success",
             f"{workflow_name} run {run_id} conclusion {run.get('conclusion')!r} is not success")


def find_full_source_validation_run(api: GitHubApi, source_sha: str) -> dict[str, Any]:
    """The newest successful canonical-main Source Validation run with full jobs and artifacts.

    Reads only run, job, and artifact metadata, never artifact bytes. Successful runs
    whose job inventory lacks one of the five platform ``py3.12`` full source
    jobs or the Ubuntu ``py3.14t`` source job — policy-only green runs — do
    not qualify as full gates, nor do runs without exactly one full-scope
    artifact per platform. When nothing qualifies, the failure explains why.
    """
    runs: list[dict[str, Any]] = []
    seen_ids: set[Any] = set()
    for event in SOURCE_GATE_EVENTS:
        for run in api.workflow_runs(
            SOURCE_VALIDATION_WORKFLOW_FILE, event=event, branch="main", head_sha=source_sha,
        ):
            if run.get("id") in seen_ids:
                continue
            seen_ids.add(run.get("id"))
            runs.append(run)
    for run in runs:
        validate_gate_run(
            api, run, SOURCE_VALIDATION_WORKFLOW_FILE, SOURCE_VALIDATION_WORKFLOW_NAME,
            source_sha, SOURCE_GATE_EVENTS)
    successful = sorted(
        (run for run in runs
         if run.get("status") == "completed" and run.get("conclusion") == "success"),
        key=lambda run: (run.get("run_number") or 0, run.get("id") or 0), reverse=True,
    )
    artifact_errors: list[str] = []
    for run in successful:
        try:
            require_passed_jobs(api, run["id"], set(CHECK_SOURCE_REQUIRED_JOBS))
        except PromotionError:
            continue
        artifacts = api.run_artifacts(run["id"])
        try:
            require_full_source_artifacts(run["id"], source_sha, artifacts)
        except PromotionError as error:
            artifact_errors.append(str(error))
            continue
        return run
    if any(run.get("status") != "completed" for run in runs):
        identifiers = ", ".join(
            f"{run.get('id')}={run.get('status')}"
            for run in runs if run.get("status") != "completed")
        raise PromotionError(
            f"Source Validation run(s) {identifiers} for {source_sha} are still running; the "
            "source gate is incomplete — rerun check-source after they finish")
    if artifact_errors:
        raise PromotionError(
            "; ".join(artifact_errors) + ". Duplicate full-scope artifacts indicate a rerun. "
            "For release, dispatch a new full Source Validation run on the same source SHA "
            f"{source_sha}: gh workflow run ci.yml --ref main -F force_full=true. "
            "Rerunning jobs is not a substitute for a new full run.")
    if successful:
        identifiers = ", ".join(str(run.get("id")) for run in successful)
        raise PromotionError(
            f"successful Source Validation run(s) {identifiers} for {source_sha} lack the "
            f"complete source job inventory {list(CHECK_SOURCE_REQUIRED_JOBS)}; a "
            "policy-only green run is not a full source gate")
    if runs:
        outcomes = ", ".join(f"{run.get('id')}={run.get('conclusion')}" for run in runs)
        raise PromotionError(
            f"Source Validation run(s) for {source_sha} did not succeed: {outcomes}")
    raise PromotionError(
        f"no successful Source Validation run exists for {source_sha} on canonical main "
        f"(events {list(SOURCE_GATE_EVENTS)}); the full source gate must pass before "
        "promotion")


def require_passed_jobs(api: GitHubApi, run_id: int, names: set[str]) -> None:
    jobs = api.run_jobs(run_id)
    for name in names:
        matches = [job for job in jobs if job.get("name") == name]
        _require(len(matches) == 1 and matches[0].get("status") == "completed"
                 and matches[0].get("conclusion") == "success",
                 f"run {run_id} required job {name!r} did not complete successfully")


def require_full_source_artifacts(
    run_id: int, source_sha: str, artifacts: list[dict[str, Any]],
) -> dict[str, dict[str, Any]]:
    """Select exactly one full-scope artifact per platform using only metadata."""
    artifacts_by_label: dict[str, dict[str, Any]] = {}
    for label in SOURCE_VALIDATION_LANES:
        expected_name = f"source-{label}-py3.12-full-{source_sha}"
        matches = [entry for entry in artifacts if entry.get("name") == expected_name]
        _require(len(matches) == 1,
                 f"Source Validation run {run_id} retains {len(matches)} {expected_name} "
                 "artifacts; the run bound by rc-context.json must carry exactly one "
                 "full-scope artifact per platform")
        artifacts_by_label[label] = matches[0]
    return artifacts_by_label


def download_and_extract_artifact(
    api: GitHubApi, artifact: dict[str, Any], destination: Path,
    seen_members: dict[str, tuple[str, str]], *, allow_nested: bool = False,
) -> None:
    """Download one artifact ZIP, verify its API digest, and safely extract it flat.

    The candidate pipeline's merged flat tree can legitimately contain the
    same file in two artifacts (the published FastDB sdist retained by the
    context job alongside a receipt's decision copy). A repeated member name
    is accepted only when its SHA-256 is identical; divergent bytes stay a
    fail-closed condition.
    """
    name = artifact.get("name")
    artifact_id = artifact.get("id")
    _require(isinstance(name, str) and isinstance(artifact_id, int), f"malformed artifact entry: {artifact!r}")
    _require(not artifact.get("expired"), f"artifact {name} has expired; rerun the candidate gates")
    digest = artifact.get("digest")
    _require(
        isinstance(digest, str) and digest.startswith("sha256:"),
        f"artifact {name} lacks an API SHA-256 digest",
    )
    status, blob = api.get_bytes(f"/repos/{api.repository}/actions/artifacts/{artifact_id}/zip")
    if status != 200:
        raise PromotionError(
            f"artifact {name} download returned HTTP {status}; expired artifacts fail closed"
        )
    actual = sha256_bytes(blob)
    _require(
        actual == digest[len("sha256:"):],
        f"artifact {name} API digest mismatch: api {digest}, actual sha256:{actual}",
    )
    archive_path = destination / f"artifact-{artifact_id}.zip"
    destination.mkdir(parents=True, exist_ok=True)
    archive_path.write_bytes(blob)
    try:
        records = zip_inventory(archive_path, label=name)
    except CandidateError as error:
        raise PromotionError(f"artifact {name} failed safe-ZIP validation: {error}") from error
    by_relative = {record["path"]: record for record in records}
    with zipfile.ZipFile(archive_path) as archive:
        for info in archive.infolist():
            if info.is_dir():
                continue
            relative = checked_relative_path(info.filename)
            _require(allow_nested or "/" not in relative,
                     f"candidate artifact {name} contains a non-flat member {relative!r}")
            record = by_relative[relative]
            member_mode = (info.external_attr >> 16) & 0xFFFF
            _require(
                not member_mode or not stat.S_ISLNK(member_mode),
                f"artifact {name} contains symbolic-link member {info.filename!r}",
            )
            previous = seen_members.get(relative)
            if previous is not None:
                owner, previous_digest = previous
                _require(
                    previous_digest == record["sha256"],
                    f"candidate member {relative!r} appears in both {owner} and {name} "
                    "with different bytes",
                )
                continue
            seen_members[relative] = (name, record["sha256"])
            target = destination / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            with archive.open(info) as source, target.open("wb") as sink:
                for chunk in iter(lambda: source.read(1 << 20), b""):
                    sink.write(chunk)
            _require(
                sha256_file(target) == record["sha256"] and target.stat().st_size == record["bytes"],
                f"extracted member {relative} does not match its verified inventory record",
            )
    archive_path.unlink()


def verify_sidecars(extraction: Path) -> list[Path]:
    files = sorted(
        path for path in extraction.rglob("*")
        if path.is_file() and not path.name.endswith(".sha256")
    )
    _require(files, "candidate extraction is empty")
    code = rc_helper.command_verify(argparse.Namespace(files=[str(path) for path in files]))
    _require(code == 0, "candidate sidecar verification failed")
    return files


def python_abi_pair(python: str) -> tuple[str, str]:
    """Map an advertised CPython row (``3.14``, ``3.14t``) to its exact tag pair."""
    value = str(python).strip().lower()
    if not re.fullmatch(r"3\.\d{2}t?", value):
        raise PromotionError(f"unsupported advertised Python row: {python!r}")
    interpreter = "cp" + value.removesuffix("t").replace(".", "")
    return interpreter, interpreter + ("t" if value.endswith("t") else "")


def wheel_platform_matches_target(platform: str, target: str) -> bool:
    """Match a wheel platform's compatibility to a candidate target."""
    if target == "x86_64-pc-windows-msvc":
        return platform == "win_amd64"
    if target == "aarch64-pc-windows-msvc":
        return platform == "win_arm64"
    architectures = {
        "x86_64-unknown-linux-gnu": ("linux", "x86_64"),
        "aarch64-unknown-linux-gnu": ("linux", "aarch64"),
        "x86_64-apple-darwin": ("macos", "x86_64"),
        "aarch64-apple-darwin": ("macos", "arm64"),
    }
    expected = architectures.get(target)
    if expected is None:
        return False
    system, architecture = expected
    prefix_ok = (
        platform.startswith(("linux_", "manylinux")) if system == "linux"
        else platform.startswith("macosx_")
    )
    return prefix_ok and (
        platform.endswith("_" + architecture)
        or (system == "macos" and platform.endswith("_universal2"))
    )


def verify_manifest_binding(
    manifest: dict[str, Any], extraction: Path, source_sha: str,
) -> tuple[dict[str, Any], int]:
    """Re-hash every manifest entry against the extracted bytes and enforce the inventory.

    Returns the verified version metadata plus the ``source_validation_run_id``
    the candidate context binds its Source Validation evidence to.
    """
    _require(manifest.get("schema") == MANIFEST_SCHEMA,
             f"rc-manifest schema {manifest.get('schema')!r} is not {MANIFEST_SCHEMA}")
    _require(manifest.get("source_commit_sha") == source_sha,
             f"rc-manifest source {manifest.get('source_commit_sha')!r} does not match {source_sha}")
    _require(manifest.get("event") == CANDIDATE_EVENT,
             f"rc-manifest event {manifest.get('event')!r} is not a {CANDIDATE_EVENT} candidate")
    _require(manifest.get("ref") == EXPECT_REF,
             f"rc-manifest ref {manifest.get('ref')!r} is not {EXPECT_REF}")
    meta = manifest.get("meta")
    _require(isinstance(meta, dict), "rc-manifest meta is missing")
    for key in ("c_two_version", "c3_version", "fastdb_version",
                "fastdb_source_rev", "fastdb_sdist_sha256"):
        _require(isinstance(meta.get(key), str) and meta[key], f"rc-manifest meta.{key} is missing")
    _require(meta.get("fastdb_source_rev") == SOURCE_VALIDATION_FASTDB_SHA,
             "rc-manifest FastDB source differs from the verified source pair")
    c_two_version, c3_version = meta["c_two_version"], meta["c3_version"]

    extracted = {}
    for path in extraction.rglob("*"):
        if path.is_file():
            _require(path.parent == extraction and path.name not in extracted,
                     f"candidate contains a non-flat or colliding member {path}")
            extracted[path.name] = path
    classified: dict[str, str] = {}

    def rehash(entries: Any, kind: str) -> list[dict[str, Any]]:
        _require(isinstance(entries, list), f"rc-manifest {kind} list is missing")
        for entry in entries:
            _require(isinstance(entry, dict), f"rc-manifest {kind} entry is not an object")
            name = entry.get("name")
            _require(isinstance(name, str), f"rc-manifest {kind} entry has no name")
            previous = classified.get(name)
            _require(previous is None, f"rc-manifest lists {name} under both {previous} and {kind}")
            classified[name] = kind
            path = extracted.get(name)
            _require(path is not None, f"rc-manifest {kind} entry {name} is absent from the artifacts")
            _require(path.stat().st_size == entry.get("bytes"),
                     f"rc-manifest {kind} entry {name} size differs from extracted bytes")
            _require(sha256_file(path) == entry.get("sha256"),
                     f"rc-manifest {kind} entry {name} SHA-256 differs from extracted bytes")
            sidecar = extracted.get(_sidecar_name(name))
            _require(sidecar is not None, f"missing sidecar for candidate file {name}")
        return entries

    cli = rehash(manifest.get("cli"), "cli")
    wheels = rehash(manifest.get("wheels"), "wheels")
    sdist = rehash(manifest.get("sdist"), "sdist")
    fastdb_proof = rehash(manifest.get("fastdb_proof"), "fastdb_proof")
    installers = rehash(manifest.get("installers"), "installers")
    receipts = rehash(manifest.get("receipts"), "receipts")

    _require({entry["name"] for entry in cli} == set(EXPECTED_CLI_NAMES),
             f"CLI inventory is not exactly {sorted(EXPECTED_CLI_NAMES)}: "
             f"{sorted(entry['name'] for entry in cli)}")
    wheel_names = [entry["name"] for entry in wheels]
    _require(len(wheel_names) == len(EXPECTED_ABI_ROWS)
             and len(set(wheel_names)) == len(EXPECTED_ABI_ROWS),
             f"expected {len(EXPECTED_ABI_ROWS)} unique wheels, found {len(wheel_names)} "
             f"({len(set(wheel_names))} unique)")
    wheel_prefix = f"c_two-{c_two_version}-"
    for name in wheel_names:
        _require(name.startswith(wheel_prefix) and name.endswith(".whl"),
                 f"wheel {name} does not match {wheel_prefix}*.whl")
    _require(len(sdist) == 1 and sdist[0]["name"] == f"c_two-{c_two_version}.tar.gz",
             f"sdist inventory must be exactly c_two-{c_two_version}.tar.gz: {sdist}")
    installer_names = {entry["name"] for entry in installers}
    if installer_names != set(EXPECTED_INSTALLER_NAMES):
        missing = sorted(set(EXPECTED_INSTALLER_NAMES) - installer_names)
        raise PromotionError(
            "candidate does not stage the canonical installer assets "
            f"{missing or sorted(installer_names)}; the Release Candidate context job must "
            "retain c3-installer.sh / c3-installer.ps1 and include them in rc-manifest.json"
        )
    proof_names = {entry["name"] for entry in fastdb_proof}
    _require(len(proof_names) == 9,
             f"expected 9 FastDB proof artifacts, found {len(proof_names)}")
    for license_name in FASTDB_LICENSE_ASSETS:
        _require(license_name in proof_names,
                 f"FastDB upstream license evidence {license_name} is missing from the candidate")
    retained_sdists = [entry for entry in fastdb_proof
                       if entry["name"].startswith("fastdb4py-") and entry["name"].endswith(".tar.gz")]
    _require(len(retained_sdists) == 1,
             f"expected exactly one retained FastDB sdist, found "
             f"{[entry['name'] for entry in retained_sdists]}")

    counts = manifest.get("counts")
    _require(isinstance(counts, dict), "rc-manifest counts are missing")
    _require(counts.get("receipts_by_prefix") == EXPECTED_RECEIPT_PREFIX_COUNTS,
             f"receipt prefix counts {counts.get('receipts_by_prefix')!r} do not match "
             f"{EXPECTED_RECEIPT_PREFIX_COUNTS}")
    actual_receipt_counts = {prefix: sum(entry["name"].startswith(prefix) for entry in receipts)
                             for prefix in EXPECTED_RECEIPT_PREFIX_COUNTS}
    _require(actual_receipt_counts == EXPECTED_RECEIPT_PREFIX_COUNTS,
             f"retained receipt prefix counts {actual_receipt_counts!r} do not match "
             f"{EXPECTED_RECEIPT_PREFIX_COUNTS}")

    unclassified = sorted(
        name for name in extracted
        if name != MANIFEST_NAME and not name.endswith(".sha256") and name not in classified
    )
    _require(not unclassified, f"unclassified candidate files: {unclassified}")

    context = _load_json_file(extraction / "rc-context.json")
    _require(isinstance(context, dict) and context.get("schema") == CONTEXT_SCHEMA,
             "rc-context.json schema mismatch")
    _require(context.get("source_commit_sha") == source_sha, "rc-context source SHA mismatch")
    _require(context.get("event") == CANDIDATE_EVENT,
             f"rc-context event {context.get('event')!r} is not a {CANDIDATE_EVENT} candidate")
    _require(context.get("ref") == EXPECT_REF,
             f"rc-context ref {context.get('ref')!r} is not {EXPECT_REF}")
    source_run_id = context.get("source_validation_run_id")
    _require(isinstance(source_run_id, int) and not isinstance(source_run_id, bool)
             and source_run_id > 0,
             "rc-context source_validation_run_id is missing or malformed; the candidate "
             "must bind the exact successful Source Validation run chosen by check-source")
    _require(context.get("c_two_version") == c_two_version, "rc-context c_two_version mismatch")
    _require(context.get("c3_version") == c3_version, "rc-context c3_version mismatch")
    _require(context.get("fastdb_version") == meta["fastdb_version"],
             "rc-context fastdb_version mismatch")
    _require(context.get("fastdb_source_rev") == meta["fastdb_source_rev"],
             "rc-context fastdb_source_rev mismatch")
    _require(context.get("pull_request_head_sha") is None,
             "rc-context records a pull_request_head_sha; only main dispatch candidates "
             "are promotable")
    versions = {"c_two_version": c_two_version, "c3_version": c3_version,
                "fastdb_version": meta["fastdb_version"],
                "fastdb_source_rev": meta["fastdb_source_rev"]}
    return versions, source_run_id


def map_wheel_rows(
    wheel_names: Iterable[str], c_two_version: str,
) -> dict[tuple[str, str, str], str]:
    """Map each wheel filename to its exact ``(target, interpreter, abi)`` row.

    Shared by the verified-manifest inventory and the registry comparison: any
    name that is not exactly one advertised row of the version's
    35-row matrix — an invalid filename, wrong distribution or
    version, an ambiguous tag pair or platform, or a duplicate row — fails
    closed. Rows are keyed by the exact ``(interpreter, abi)`` tag pair so
    the GIL ``cp314-cp314`` and free-threaded ``cp314-cp314t`` builds stay
    distinct.
    """
    allowed_pairs = set(PYTHON_ABI_PAIRS.values())
    expected_rows = {(target, *PYTHON_ABI_PAIRS[python]) for target, python in EXPECTED_ABI_ROWS}
    by_row: dict[tuple[str, str, str], str] = {}
    for name in wheel_names:
        try:
            distribution, version, _, tags = parse_wheel_filename(name)
        except (InvalidWheelFilename, ValueError) as error:
            raise PromotionError(f"wheel {name} is not a valid wheel filename: {error}") from error
        _require(distribution.replace("_", "-").lower() == "c-two",
                 f"unexpected wheel distribution {name}")
        _require(str(version) == c_two_version,
                 f"wheel {name} version {version} is not {c_two_version}")
        pairs = {(tag.interpreter, tag.abi) for tag in tags} & allowed_pairs
        tag_pairs = sorted(f"{tag.interpreter}-{tag.abi}" for tag in tags)
        _require(len(pairs) == 1,
                 f"wheel {name} does not carry exactly one advertised ABI pair: {tag_pairs}")
        pair = pairs.pop()
        targets = {target for target in CLI_TARGETS
                   if any(wheel_platform_matches_target(tag.platform, target) for tag in tags)}
        _require(len(targets) == 1,
                 f"wheel {name} does not map to exactly one CLI target platform")
        key = (targets.pop(), *pair)
        _require(key in expected_rows,
                 f"wheel {name} is outside the expected platform/ABI matrix")
        _require(key not in by_row,
                 f"duplicate wheel row for {key[0]} {key[1]}-{key[2]}")
        by_row[key] = name
    return by_row


def verify_wheel_rows(wheels: list[dict[str, Any]], c_two_version: str) -> dict[str, str]:
    """Require 35 wheel rows: six ABIs per target except Windows ARM64's five."""
    by_row = map_wheel_rows((entry["name"] for entry in wheels), c_two_version)
    python_by_pair = {pair: python for python, pair in PYTHON_ABI_PAIRS.items()}
    coverage = EXPECTED_ABI_ROWS
    have = {(target, python_by_pair[(interpreter, abi)])
            for target, interpreter, abi in by_row}
    _require(have == coverage,
             f"wheel rows are not the exact 35-row matrix; missing {sorted(coverage - have)}, "
             f"unexpected {sorted(have - coverage)}")
    return {f"{target}/{python}": by_row[(target, *PYTHON_ABI_PAIRS[python])]
            for target, python in coverage}


def _receipt_artifact(receipt_name: str, artifacts: Any, predicate, label: str) -> dict[str, Any]:
    _require(isinstance(artifacts, list), f"receipt {receipt_name} has no artifact inventory")
    matches = [entry for entry in artifacts if isinstance(entry, dict) and predicate(entry)]
    _require(len(matches) == 1, f"receipt {receipt_name} does not bind exactly one {label}")
    return matches[0]


def _verify_lifecycle_cleanup(receipt_name: str, payload: dict[str, Any]) -> None:
    """Lifecycle receipts must prove orderly shutdown and full cleanup."""
    base = payload.get("base_install") or {}
    _require(base.get("status") == "passed"
             and base.get("transports") == ["thread", "ipc"]
             and base.get("optional_dependencies_present") is False
             and base.get("optional_modules_loaded") is False,
             f"receipt {receipt_name} does not prove a dependency-free base installation")
    checks = payload.get("checks")
    _require(isinstance(checks, list) and len(checks) == 2,
             f"receipt {receipt_name} does not record both consumer checks")
    transports = set()
    for check in checks:
        _require(isinstance(check, dict) and check.get("status") == "passed",
                 f"receipt {receipt_name} has a non-passing consumer check")
        transport = check.get("transport")
        transports.add(transport)
        _require(transport in ("direct", "relay"),
                 f"receipt {receipt_name} check transport {transport!r} is invalid")
        _require(check.get("observed_mode") == ("ipc" if transport == "direct" else "http"),
                 f"receipt {receipt_name} {transport} observed_mode is inconsistent")
        _require(check.get("held_view_invalidated") is True,
                 f"receipt {receipt_name} {transport} did not invalidate held views")
    _require(transports == {"direct", "relay"},
             f"receipt {receipt_name} transports are not direct+relay")
    _require(payload.get("borrowed_inputs_invalidated") == 4,
             f"receipt {receipt_name} borrowed_inputs_invalidated "
             f"{payload.get('borrowed_inputs_invalidated')!r} is not 4")
    cleanup = payload.get("cleanup")
    _require(isinstance(cleanup, dict), f"receipt {receipt_name} has no cleanup record")
    for key in ("host_orderly_shutdown", "processes_exited", "temporary_directory_removed"):
        _require(cleanup.get(key) is True, f"receipt {receipt_name} cleanup.{key} is not true")
    _require(not cleanup.get("errors"), f"receipt {receipt_name} cleanup errors {cleanup.get('errors')!r}")


def _validate_fastdb_decision(receipt_name: str, decision: Any, context: dict[str, Any]) -> None:
    """Bind one receipt's FastDB decision to the verified published metadata."""
    _require(isinstance(decision, dict) and "sha256" in decision and "mode" in decision,
             f"receipt {receipt_name} has no FastDB decision")
    mode = decision["mode"]
    listing = context["fastdb_listing"]
    retained_sdist = context["retained_fastdb_sdist"]
    if mode == "wheel":
        _require(listing.get(decision.get("filename")) == decision.get("sha256"),
                 f"receipt {receipt_name} fastdb wheel {decision.get('filename')} is not the "
                 "verified published file from rc-fastdb-public.json")
    elif mode == "sdist":
        _require(retained_sdist["sha256"] == decision.get("sha256"),
                 f"receipt {receipt_name} fastdb sdist sha256 does not match the single "
                 "retained verified sdist")
        built = decision.get("built_wheel")
        _require(isinstance(built, dict) and "name" in built and "sha256" in built,
                 f"receipt {receipt_name} sdist decision has no built_wheel")
        _require(built.get("derived_from_sdist_sha256") == decision.get("sha256"),
                 f"receipt {receipt_name} built-wheel derivation does not match its own sdist sha256")
    else:
        raise PromotionError(f"receipt {receipt_name} fastdb decision has no wheel/sdist mode")


def _verify_windows_architecture(receipt_name: str, payload: dict[str, Any], python_platform: str) -> None:
    facts = payload.get("native_architecture", {})
    _require(facts.get("python_platform") == python_platform,
             f"{receipt_name} did not use native {python_platform} Python")
    expected = {"win-amd64": 0x8664, "win-arm64": 0xAA64}[python_platform]
    machines = facts.get("pe_machines")
    _require(isinstance(machines, dict) and machines and set(machines.values()) == {expected},
             f"{receipt_name} PE machines do not match {python_platform}")
    for artifact in payload["artifacts"]:
        name = artifact["name"]
        _require(any(member == name or member.startswith(name + "/") for member in machines),
                 f"{receipt_name} lacks native PE evidence for {name}")


def _bind_smoke_artifacts(
    receipt_name: str, payload: dict[str, Any], manifest: dict[str, Any],
    context: dict[str, Any], *, cli_name: str | None, wheel_name: str | None,
    decision: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Bind a lifecycle receipt's wheel/CLI/FastDB artifact entries to retained bytes.

    ABI-row receipts (and the sdist proof) carry an explicit FastDB decision;
    plain smoke receipts record only the consumed artifact entry, which must
    be one of the retained FastDB proof wheels.
    """
    artifacts = payload.get("artifacts")
    ctwo = _receipt_artifact(
        receipt_name, artifacts,
        lambda entry: entry.get("distribution") == "c-two", "c-two wheel")
    fastdb = _receipt_artifact(
        receipt_name, artifacts,
        lambda entry: entry.get("distribution") == "fastdb4py", "fastdb4py wheel")
    wheels_by_identity = {(entry["name"], entry["sha256"]): entry for entry in manifest["wheels"]}
    identity = (ctwo.get("name"), ctwo.get("sha256"))
    _require(identity in wheels_by_identity,
             f"receipt {receipt_name} c-two wheel {identity[0]} is not a retained wheel "
             "with those bytes")
    if wheel_name is not None:
        _require(ctwo.get("name") == wheel_name,
                 f"receipt {receipt_name} consumed wheel {ctwo.get('name')!r}, expected {wheel_name}")
    if cli_name is not None:
        cli = _receipt_artifact(
            receipt_name, artifacts,
            lambda entry: str(entry.get("name", "")).startswith("c3-"), "c3 executable")
        _require(cli.get("name") == cli_name,
                 f"receipt {receipt_name} CLI artifact {cli.get('name')!r} is not {cli_name}")
        cli_inventory = {entry["name"]: entry["sha256"] for entry in manifest["cli"]}
        _require(cli_inventory.get(cli_name) == cli.get("sha256"),
                 f"receipt {receipt_name} CLI bytes do not match the retained {cli_name}")
        if cli_name.endswith("-pc-windows-msvc.exe"):
            _verify_windows_architecture(receipt_name, payload,
                                         "win-arm64" if "aarch64" in cli_name else "win-amd64")
    if decision is not None:
        _validate_fastdb_decision(receipt_name, decision, context)
        if decision["mode"] == "wheel":
            expected = (decision.get("filename"), decision.get("sha256"))
        else:
            built = decision["built_wheel"]
            expected = (built.get("name"), built.get("sha256"))
        _require((fastdb.get("name"), fastdb.get("sha256")) == expected,
                 f"receipt {receipt_name} consumed fastdb wheel differs from its decision")
    else:
        proof_identities = {(entry["name"], entry["sha256"]) for entry in manifest["fastdb_proof"]}
        _require((fastdb.get("name"), fastdb.get("sha256")) in proof_identities,
                 f"receipt {receipt_name} consumed fastdb wheel {fastdb.get('name')} is not a "
                 "retained proof wheel with those bytes")
    return ctwo


def verify_candidate_receipts(
    extraction: Path, manifest: dict[str, Any], source_sha: str,
    wheel_by_row: dict[str, str],
) -> None:
    """Re-validate the receipts that bind the retained bytes to executed tests."""
    context: dict[str, Any] = {
        "fastdb_listing": {},
        "retained_fastdb_sdist": next(
            entry for entry in manifest["fastdb_proof"]
            if entry["name"].startswith("fastdb4py-") and entry["name"].endswith(".tar.gz")),
    }
    listing_payload = _load_json_file(extraction / "rc-fastdb-public.json")
    if isinstance(listing_payload, dict):
        context["fastdb_listing"] = {
            file["filename"]: file["sha256"]
            for file in listing_payload.get("files", [])
            if isinstance(file, dict) and "filename" in file and "sha256" in file
        }
    _require(context["fastdb_listing"], "rc-fastdb-public.json has no verified published listing")
    _require(
        context["fastdb_listing"].get(context["retained_fastdb_sdist"]["name"])
        == context["retained_fastdb_sdist"]["sha256"],
        "retained FastDB sdist does not match the verified published listing",
    )
    built_identities: set[tuple[str, str]] = set()
    proof_wheels = {entry["name"]: entry["sha256"] for entry in manifest["fastdb_proof"]
                    if entry["name"].endswith(".whl")}
    _require(len(proof_wheels) == len(CLI_TARGETS),
             "FastDB proof wheels must cover every CLI target exactly once on cp312")

    def load(name: str) -> dict[str, Any]:
        payload = _load_json_file(extraction / name)
        _require(isinstance(payload, dict), f"receipt {name} is not an object")
        return payload

    # Provisioning wheels and per-ABI wheels are separate builds. Bind each
    # retained provisioning wheel to its own source-checked decision receipt.
    for receipt_name in ("fastdb-sdist.json", *(f"fastdb-{target}.json" for target in CLI_TARGETS)):
        decision = load(receipt_name)
        _require(decision.get("schema") == "c-two.fastdb-release.resolution.v1"
                 and decision.get("project") == "fastdb4py"
                 and decision.get("version") == manifest["meta"]["fastdb_version"],
                 f"{receipt_name} provisioning identity mismatch")
        if receipt_name == "fastdb-sdist.json":
            _require(decision.get("mode") == "sdist" and
                     decision.get("sha256") == context["retained_fastdb_sdist"]["sha256"],
                     "source-only FastDB receipt does not bind the retained sdist")
        else:
            _validate_fastdb_decision(receipt_name, decision, context)
            if decision["mode"] == "sdist":
                built = decision["built_wheel"]
                built_identities.add((built["name"], built["sha256"]))
                name, digest = built["name"], built["sha256"]
            else:
                name, digest = decision["filename"], decision["sha256"]
            _require(name in proof_wheels and proof_wheels.pop(name) == digest,
                     f"FastDB proof wheel {name} is not a distinct retained wheel bound by {receipt_name}")
            try:
                distribution, version, _, tags = parse_wheel_filename(name)
            except InvalidWheelFilename as error:
                raise PromotionError(f"invalid FastDB proof wheel {name}: {error}") from error
            target = receipt_name.removeprefix("fastdb-").removesuffix(".json")
            _require(distribution == "fastdb4py" and str(version) == manifest["meta"]["fastdb_version"],
                     f"FastDB proof wheel {name} distribution/version mismatch")
            _require(any((tag.interpreter, tag.abi) == ("cp312", "cp312")
                         and wheel_platform_matches_target(tag.platform, target) for tag in tags),
                     f"FastDB proof wheel {name} does not support the cp312 target {target}")

    # 35 executed ABI rows: installed-wheel-smoke receipts bound to their row.
    rows: dict[tuple[str, str], str] = {}
    for path in sorted(extraction.glob("abi-*.json")):
        payload = load(path.name)
        _require(payload.get("schema") == SMOKE_SCHEMA,
                 f"receipt {path.name} schema {payload.get('schema')!r} is not {SMOKE_SCHEMA}")
        _require(payload.get("status") == "passed",
                 f"receipt {path.name} status {payload.get('status')!r} is not passed")
        _require(payload.get("source_commit_sha") == source_sha,
                 f"receipt {path.name} source_commit_sha is not the candidate source")
        row = payload.get("row")
        _require(isinstance(row, dict)
                 and row.get("target") in CLI_TARGETS and row.get("python") in EXPECTED_PYTHONS
                 and (row.get("target"), row.get("python")) in EXPECTED_ABI_ROWS,
                 f"receipt {path.name} row {row!r} is outside the expected matrix")
        key = (row["target"], row["python"])
        _require(key not in rows, f"duplicate ABI row receipt for {key[0]}/{key[1]}")
        rows[key] = path.name
        expected_pair = python_abi_pair(key[1])
        recorded_abi = payload.get("wheel_abi")
        _require(isinstance(recorded_abi, dict)
                 and (recorded_abi.get("interpreter"), recorded_abi.get("abi")) == expected_pair,
                 f"receipt {path.name} wheel_abi {recorded_abi!r} differs from row python "
                 f"{expected_pair[0]}-{expected_pair[1]}")
        _verify_lifecycle_cleanup(path.name, payload)
        decision = payload.get("fastdb")
        ctwo = _bind_smoke_artifacts(
            path.name, payload, manifest, context,
            cli_name=f"c3-{key[0]}{'.exe' if key[0] in WINDOWS_TARGETS else ''}",
            wheel_name=wheel_by_row[f"{key[0]}/{key[1]}"],
            decision=decision,
        )
        _, _, _, wheel_tags = parse_wheel_filename(ctwo["name"])
        _require(any((tag.interpreter, tag.abi) == expected_pair for tag in wheel_tags),
                 f"receipt {path.name} wheel does not carry the row's "
                 f"{expected_pair[0]}-{expected_pair[1]} tag")
        _require(any(wheel_platform_matches_target(tag.platform, key[0]) for tag in wheel_tags),
                 f"receipt {path.name} wheel platform does not match row target {key[0]}")
        if decision["mode"] == "sdist":
            built = decision["built_wheel"]
            built_identities.add((built["name"], built["sha256"]))
    expected_rows = EXPECTED_ABI_ROWS
    _require(set(rows) == expected_rows,
             f"ABI rows cover {len(rows)} of {len(expected_rows)}; missing "
             f"{sorted(expected_rows - set(rows))}")

    # 6 lifecycle smokes on the cp312 wheel of each target.
    for target in CLI_TARGETS:
        receipt_name = f"smoke-{target}.json"
        payload = load(receipt_name)
        _require(payload.get("schema") == SMOKE_SCHEMA, f"{receipt_name} schema mismatch")
        _require(payload.get("status") == "passed",
                 f"{receipt_name} status {payload.get('status')!r} is not passed")
        _require(payload.get("source_commit_sha") == source_sha,
                 f"{receipt_name} source_commit_sha is not the candidate source")
        _verify_lifecycle_cleanup(receipt_name, payload)
        _bind_smoke_artifacts(
            receipt_name, payload, manifest, context,
            cli_name=f"c3-{target}{'.exe' if target in WINDOWS_TARGETS else ''}",
            wheel_name=wheel_by_row[f"{target}/3.12"],
        )

    # Windows standard-user lifecycle/cleanup, ordinary + wrapper.
    for target in WINDOWS_TARGETS:
        receipt_name = f"standard-user-{target}.json"
        standard = load(receipt_name)
        _require(standard.get("schema") == SMOKE_SCHEMA, "standard-user receipt schema mismatch")
        _require(standard.get("status") == "passed",
                 f"standard-user receipt status {standard.get('status')!r} is not passed")
        _require(standard.get("source_commit_sha") == source_sha,
                 "standard-user receipt source_commit_sha is not the candidate source")
        identity = standard.get("identity")
        _require(isinstance(identity, dict) and identity.get("administrator") is False,
                 "standard-user receipt was not run by a non-administrator identity")
        _verify_lifecycle_cleanup(receipt_name, standard)
        _bind_smoke_artifacts(
            receipt_name, standard, manifest, context,
            cli_name=f"c3-{target}.exe",
            wheel_name=wheel_by_row[f"{target}/3.12"],
        )
        wrapper = load(receipt_name + ".wrapper.json")
        _require(wrapper.get("schema") == STANDARD_USER_WRAPPER_SCHEMA,
                 "standard-user wrapper schema mismatch")
        _require(wrapper.get("status") == "passed",
                 f"standard-user wrapper status {wrapper.get('status')!r} is not passed")
        _require(not wrapper.get("errors"), f"standard-user wrapper errors {wrapper.get('errors')!r}")
        wrapper_cleanup = wrapper.get("cleanup")
        _require(isinstance(wrapper_cleanup, dict), "standard-user wrapper has no cleanup record")
        for key in ("processes_exited", "workspace_removed", "account_removed"):
            _require(wrapper_cleanup.get(key) is True,
                     f"standard-user wrapper cleanup.{key} is not true")

    # sdist member-tree inspection plus the isolated repaired-wheel proof.
    sdist_entry = manifest["sdist"][0]
    inspect = load("sdist-inspect.json")
    _require(inspect.get("schema") == SDIST_INSPECT_SCHEMA, "sdist-inspect schema mismatch")
    _require(inspect.get("status") == "passed", "sdist-inspect status is not passed")
    _require(inspect.get("archive") == sdist_entry["name"], "sdist-inspect archive mismatch")
    _require(inspect.get("source_commit_sha") == source_sha,
             "sdist-inspect source_commit_sha is not the candidate source")
    proof = load("sdist-proof.json")
    _require(proof.get("schema") == SDIST_PROOF_SCHEMA, "sdist-proof schema mismatch")
    _require(proof.get("status") == "executed", "sdist-proof status is not executed")
    _require(proof.get("source_commit_sha") == source_sha,
             "sdist-proof source_commit_sha is not the candidate source")
    proven = proof.get("sdist")
    _require(isinstance(proven, dict) and proven.get("name") == sdist_entry["name"]
             and proven.get("sha256") == sdist_entry["sha256"],
             "sdist-proof does not bind the manifest sdist bytes")
    isolated = proof.get("isolated_wheel")
    _require(isinstance(isolated, dict) and isolated.get("name") and isolated.get("sha256"),
             "sdist-proof does not record the isolated repaired wheel")
    _require(proof.get("link_mode") == "system",
             "sdist-proof link_mode is not the packaged-consumer system mode")
    _require(proof.get("standalone_import") == "passed",
             "sdist-proof standalone import did not pass")
    lifecycle = proof.get("lifecycle")
    _require(isinstance(lifecycle, dict) and lifecycle.get("status") == "passed",
             "sdist-proof embedded lifecycle did not pass")
    _require(lifecycle.get("schema") == SMOKE_SCHEMA
             and lifecycle.get("source_commit_sha") == source_sha,
             "sdist-proof embedded lifecycle schema/source mismatch")
    _verify_lifecycle_cleanup("sdist-proof lifecycle", lifecycle)
    artifacts = lifecycle.get("artifacts")
    derived = _receipt_artifact("sdist-proof", artifacts,
                               lambda entry: entry.get("distribution") == "c-two", "derived wheel")
    _require((derived.get("name"), derived.get("sha256")) == (isolated["name"], isolated["sha256"]),
             "sdist-proof lifecycle did not consume the recorded derived wheel")
    cli_name = "c3-x86_64-unknown-linux-gnu"
    cli = _receipt_artifact("sdist-proof", artifacts,
                           lambda entry: entry.get("name") == cli_name, "retained CLI")
    expected_cli = next(entry for entry in manifest["cli"] if entry["name"] == cli_name)
    _require(cli.get("sha256") == expected_cli["sha256"], "sdist-proof CLI bytes differ")
    decision = proof.get("fastdb")
    _validate_fastdb_decision("sdist-proof.json", decision, context)
    fastdb = _receipt_artifact("sdist-proof", artifacts,
                              lambda entry: entry.get("distribution") == "fastdb4py", "FastDB wheel")
    _require(decision["mode"] == "wheel" and
             (fastdb.get("name"), fastdb.get("sha256")) == (decision["filename"], decision["sha256"]),
             "sdist-proof lifecycle FastDB bytes differ")
    sdk = proof.get("sdk") or {}
    _require(sdk.get("version") == SOURCE_VALIDATION_FASTDB_CORE_VERSION
             and sdk.get("source_sha") == SOURCE_VALIDATION_FASTDB_SHA,
             "sdist-proof Core SDK source/version mismatch")
    bundled = proof.get("bundled_fastdb_libs")
    elf = proof.get("elf_dynamic") or {}
    runpath = elf.get("runpath")
    _require(isinstance(bundled, list) and bundled
             and any("libfastdb" in entry for entry in elf.get("needed", [])),
             "sdist-proof has no bundled FastDB runtime dependency")
    _require(isinstance(runpath, str) and runpath
             and all(part.startswith("$ORIGIN/") for part in runpath.split(":")),
             "sdist-proof runtime search paths are not wheel-relative")

    # Every retained FastDB wheel must be published or receipt-built.
    for entry in manifest["fastdb_proof"]:
        if not entry["name"].endswith(".whl"):
            continue
        published = context["fastdb_listing"].get(entry["name"]) == entry["sha256"]
        referenced = (entry["name"], entry["sha256"]) in built_identities
        _require(published or referenced,
                 f"retained FastDB wheel {entry['name']} is neither the verified published "
                 "file nor a receipt-built wheel")


def expected_source_full_gates(target_platform: str) -> list[str]:
    """Derive the full-scope gate inventory from the trusted runner source.

    The promoting checkout is current ``main`` code; its ``source_validation.py``
    defines exactly which gates a full run must have executed and passed.
    """
    runner_source = REPO_ROOT / "tools" / "ci" / "source_validation.py"
    try:
        spec = importlib.util.spec_from_file_location("c_two_source_validation_gates", runner_source)
        _require(spec is not None and spec.loader is not None,
                 f"cannot load trusted runner source {runner_source}")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        names = [name for name, _, _ in module.gates(
            "python", Path("."), "full", target_platform=target_platform)]
        _require(names and len(names) == len(set(names)),
                 f"trusted {target_platform} full-scope gate inventory is empty or duplicated")
        return names
    except OSError as error:
        raise PromotionError(f"cannot read trusted runner source {runner_source}: {error}") from error


def _source_fastdb_pin() -> str:
    """Read the frozen FastDB pin from the trusted ci.yml."""
    workflow = REPO_ROOT / ".github" / "workflows" / "ci.yml"
    try:
        text = workflow.read_text(encoding="utf-8")
    except OSError as error:
        raise PromotionError(f"cannot read {workflow}: {error}") from error
    match = re.search(r"^\s*FASTDB_SOURCE_SHA:\s*([0-9a-f]{40})\s*$", text, re.MULTILINE)
    _require(match is not None, "ci.yml has no FASTDB_SOURCE_SHA pin")
    pin = match.group(1)
    _require(pin == SOURCE_VALIDATION_FASTDB_SHA,
             f"ci.yml FastDB pin {pin} differs from the frozen promotion pin "
             f"{SOURCE_VALIDATION_FASTDB_SHA}; update the promotion contract deliberately")
    return pin


def _verify_source_wheel_receipt(
    receipt_name: str, payload: Any, *, expect_admin: bool | None,
    source_c3_sha: str, c3_name: str,
) -> None:
    _require(isinstance(payload, dict) and payload.get("schema") == SMOKE_SCHEMA,
             f"{receipt_name} schema mismatch")
    _require(payload.get("status") == "passed", f"{receipt_name} status is not passed")
    identity = payload.get("identity")
    _require(isinstance(identity, dict) and "administrator" in identity
             and identity["administrator"] is expect_admin,
             f"{receipt_name} identity {identity!r}, expected administrator={expect_admin}")
    _verify_lifecycle_cleanup(receipt_name, payload)
    artifacts = payload.get("artifacts")
    _require(isinstance(artifacts, list), f"{receipt_name} artifacts are missing")
    recorded = [item for item in artifacts
                if isinstance(item, dict) and item.get("name") == c3_name]
    _require(len(recorded) == 1, f"{receipt_name} does not bind exactly one c3 executable")
    _require(recorded[0].get("sha256") == source_c3_sha,
             f"{receipt_name} c3 bytes do not match the source lane's own {c3_name}")


def _verify_python_run_receipt(receipt_name: str, payload: Any, source_c3_sha: str) -> None:
    """Verify fields emitted by tools/dev/test_python.py after its strict partition."""
    _require(isinstance(payload, dict) and payload.get("schema") == "c-two.python-test-run.v1",
             f"{receipt_name} schema mismatch")
    _require(payload.get("status") == "passed", f"{receipt_name} status is not passed")
    inputs = payload.get("inputs")
    _require(isinstance(inputs, dict) and inputs.get("c3_sha256") == source_c3_sha,
             f"{receipt_name} input c3 hash does not match the source lane's own build")
    steps = payload.get("steps")
    expected = {"collect-full", "collect-ordinary", "collect-portable", "collect-typescript",
                "ordinary", "portable", "typescript"}
    _require(isinstance(steps, list) and len(steps) == len(expected)
             and all(isinstance(step, dict) and isinstance(step.get("id"), str) for step in steps)
             and {step["id"] for step in steps} == expected,
             f"{receipt_name} collection/execution steps differ from the complete Python partition")
    for step in steps:
        _require(step.get("status") == "passed" and step.get("exit_code") == 0
                 and step.get("process_exited") is True and step.get("cleanup_confirmed") is True
                 and not step.get("validation_error") and not step.get("error")
                 and not step.get("cleanup_error"),
                 f"{receipt_name} step {step['id']} did not pass with confirmed cleanup")


def verify_source_validation(
    api: GitHubApi, source_sha: str, source_run: dict[str, Any], download_root: Path,
) -> dict[str, Any]:
    """Re-validate full-scope source evidence for all five platform/runner lanes.

    Every lane's full-scope artifact must come from the single Source
    Validation run bound by the candidate context through
    ``source_validation_run_id``; evidence is never mixed across runs.
    """
    fastdb_pin = _source_fastdb_pin()
    run_id = source_run.get("id")
    require_passed_jobs(api, run_id, set(CHECK_SOURCE_REQUIRED_JOBS))
    artifacts_by_label = require_full_source_artifacts(run_id, source_sha, api.run_artifacts(run_id))

    validated: dict[str, Any] = {}
    for label, target_platform in SOURCE_VALIDATION_LANES.items():
        expected_gates = expected_source_full_gates(target_platform)
        artifact = artifacts_by_label[label]
        destination = download_root / "source-validation" / label
        # Each runner image extracts into its own directory; the duplicate
        # check is scoped to one destination.
        seen_members: dict[str, tuple[str, str]] = {}
        download_and_extract_artifact(api, artifact, destination, seen_members, allow_nested=True)

        evidence = _load_json_file(destination / "run-evidence.json")
        _require(isinstance(evidence, dict) and evidence.get("schema") == SOURCE_EVIDENCE_SCHEMA,
                 f"{label} run-evidence schema mismatch")
        _require(evidence.get("status") == "passed",
                 f"{label} run-evidence status {evidence.get('status')!r} is not passed")
        _require(evidence.get("scope") == "full", f"{label} evidence scope is not full")
        _require(evidence.get("platform") == target_platform,
                 f"{label} evidence platform {evidence.get('platform')!r} is not {target_platform}")
        runner = evidence.get("runner")
        _require(isinstance(runner, dict) and runner.get("requested_label") == label,
                 f"{label} evidence runner label mismatch: {runner!r}")
        if label == "windows-11-arm":
            _require(runner.get("python_platform") == "win-arm64",
                     f"{label} source Python is not native ARM64")
            rustc = evidence.get("toolchains", {}).get("rustc", {})
            _require(rustc.get("exit_code") == 0
                     and "host: aarch64-pc-windows-msvc" in rustc.get("output", "").splitlines(),
                     f"{label} source Rust host is not native ARM64")
        sources = evidence.get("sources")
        _require(isinstance(sources, dict), f"{label} evidence sources are missing")
        for name, expected_sha in (("c-two", source_sha), ("fastdb", fastdb_pin)):
            source = sources.get(name)
            _require(isinstance(source, dict) and source.get("verified") is True,
                     f"{label} evidence {name} source is not verified")
            _require(source.get("actual_sha") == expected_sha,
                     f"{label} evidence {name} SHA {source.get('actual_sha')!r} is not {expected_sha}")
            _require(source.get("expected_sha") == expected_sha,
                     f"{label} evidence {name} expected_sha differs from the frozen pin")

        # Full gate inventory: every applicable gate ran and passed; nothing
        # was skipped, failed, timed out, or left not_run.
        applicable = evidence.get("applicable_gates")
        _require(isinstance(applicable, list) and applicable
                 and all(isinstance(name, str) for name in applicable)
                 and len(applicable) == len(set(applicable)),
                 f"{label} evidence has no applicable gate inventory")
        _require(set(applicable) == set(expected_gates),
                 f"{label} applicable gates differ from the trusted full-scope inventory: "
                 f"missing {sorted(set(expected_gates) - set(applicable))}, "
                 f"unexpected {sorted(set(applicable) - set(expected_gates))}")
        steps = evidence.get("steps")
        _require(isinstance(steps, list) and len(steps) == len(applicable)
                 and all(isinstance(step, dict) and isinstance(step.get("id"), str) for step in steps),
                 f"{label} evidence has missing, duplicate or malformed step records")
        step_status = {step.get("id"): step.get("status") for step in steps
                       if isinstance(step, dict)}
        _require(set(step_status) == set(applicable),
                 f"{label} recorded steps differ from the applicable gate inventory")
        not_passed = sorted(name for name, status in step_status.items() if status != "passed")
        _require(not not_passed, f"{label} gates are not all passed: {not_passed}")

        # Every internal artifact the runner recorded must be present and
        # byte-identical: CLI, wheels, receipts, JUnit, logs, cleanup evidence.
        recorded_artifacts = evidence.get("artifacts")
        _require(isinstance(recorded_artifacts, list) and recorded_artifacts,
                 f"{label} evidence records no internal artifacts")
        recorded_paths: set[str] = set()
        for record in recorded_artifacts:
            _require(isinstance(record, dict) and isinstance(record.get("path"), str),
                     f"{label} malformed internal artifact record {record!r}")
            try:
                relative = checked_relative_path(record["path"])
            except CandidateError as error:
                raise PromotionError(f"{label} invalid internal artifact path: {error}") from error
            _require(relative not in recorded_paths, f"{label} duplicate internal artifact {relative}")
            recorded_paths.add(relative)
            target = destination / relative
            _require(target.is_file(),
                     f"{label} recorded artifact {record['path']} is absent from the ZIP")
            _require(sha256_file(target) == record.get("sha256"),
                     f"{label} recorded artifact {record['path']} SHA-256 mismatch")
            _require(target.stat().st_size == record.get("bytes"),
                     f"{label} recorded artifact {record['path']} size mismatch")

        c3_name = "c3.exe" if target_platform == "windows" else "c3"
        c3_path = f"cli/{c3_name}"
        required_paths = {c3_path, MATRIX_RECEIPT_NAME, TYPESCRIPT_RECEIPT_NAME,
                          PYTHON_RUN_RECEIPT_NAME, SOURCE_WHEEL_RECEIPT}
        if target_platform == "windows":
            required_paths.update({SOURCE_STANDARD_USER_RECEIPT, SOURCE_STANDARD_USER_WRAPPER})
        _require(required_paths <= recorded_paths,
                 f"{label} required source artifacts are not hash-bound: {sorted(required_paths - recorded_paths)}")
        source_c3 = destination / c3_path
        _require(source_c3.is_file(), f"{label} is missing its {c3_path} build")
        source_c3_sha = sha256_file(source_c3)
        _verify_python_run_receipt(
            f"{label}/{PYTHON_RUN_RECEIPT_NAME}",
            _load_json_file(destination / PYTHON_RUN_RECEIPT_NAME), source_c3_sha)

        for receipt_name, validator in (
            (MATRIX_RECEIPT_NAME, portable_matrix_receipt),
            (TYPESCRIPT_RECEIPT_NAME, typescript_receipt),
        ):
            receipt_path = destination / receipt_name
            _require(receipt_path.is_file(), f"{label} is missing {receipt_name}")
            try:
                validator.load_and_validate_receipt(receipt_path, expected_stage="development")
            except Exception as error:  # noqa: BLE001 - validator errors are fail-closed evidence
                raise PromotionError(
                    f"{label} {receipt_name} failed the reviewed receipt validator: {error}"
                ) from error
        matrix = _load_json_file(destination / MATRIX_RECEIPT_NAME)
        typescript = _load_json_file(destination / TYPESCRIPT_RECEIPT_NAME)
        _require(len(matrix.get("rows", [])) == 18,
                 f"{label} portable matrix has {len(matrix.get('rows', []))} rows, expected 18")
        _require(len(typescript.get("rows", [])) == 12,
                 f"{label} TypeScript receipt has {len(typescript.get('rows', []))} rows, expected 12")
        if label == "windows-11-arm":
            _require(typescript.get("runtime", {}).get("arch") == "arm64",
                     f"{label} TypeScript did not use native ARM64 Node")
        # Bind the source debug c3 to its own matrix and wheel receipts;
        # released candidate executables have their separate byte identities.
        relay_c3 = {row.get("c3_sha256") for row in matrix.get("rows", []) if row.get("c3_sha256")}
        _require(relay_c3 == {source_c3_sha},
                 f"{label} portable matrix c3 hashes do not match the source {c3_name} build")
        _require(typescript.get("packages", {}).get("c3_sha256") == source_c3_sha,
                 f"{label} TypeScript receipt c3 hash does not match the source {c3_name} build")

        ordinary = _load_json_file(destination / SOURCE_WHEEL_RECEIPT)
        _verify_source_wheel_receipt(
            f"{label}/{SOURCE_WHEEL_RECEIPT}", ordinary,
            expect_admin=True if target_platform == "windows" else None,
            source_c3_sha=source_c3_sha, c3_name=c3_name)
        if target_platform == "windows":
            python_platform = "win-arm64" if label == "windows-11-arm" else "win-amd64"
            _verify_windows_architecture(f"{label}/{SOURCE_WHEEL_RECEIPT}", ordinary, python_platform)
            standard = _load_json_file(destination / SOURCE_STANDARD_USER_RECEIPT)
            _verify_source_wheel_receipt(
                f"{label}/{SOURCE_STANDARD_USER_RECEIPT}", standard,
                expect_admin=False, source_c3_sha=source_c3_sha, c3_name=c3_name)
            _verify_windows_architecture(f"{label}/{SOURCE_STANDARD_USER_RECEIPT}", standard, python_platform)
            wrapper = _load_json_file(destination / SOURCE_STANDARD_USER_WRAPPER)
            _require(isinstance(wrapper, dict) and wrapper.get("schema") == STANDARD_USER_WRAPPER_SCHEMA,
                     f"{label} standard-user wrapper schema mismatch")
            _require(wrapper.get("status") == "passed",
                     f"{label} standard-user wrapper status is not passed")
            _require(not wrapper.get("errors"), f"{label} standard-user wrapper errors present")
            wrapper_cleanup = wrapper.get("cleanup")
            _require(isinstance(wrapper_cleanup, dict), f"{label} standard-user wrapper has no cleanup")
            for key in ("processes_exited", "workspace_removed", "account_removed"):
                _require(wrapper_cleanup.get(key) is True,
                         f"{label} standard-user wrapper cleanup.{key} is not true")

        validated[label] = {
            "platform": target_platform,
            "run_id": source_run.get("id"),
            "run_url": source_run.get("html_url"),
            "artifact": artifact.get("name"),
            "gates_passed": len(applicable),
            "source_c3_sha256": source_c3_sha,
        }
    return validated


def stage_github_release_assets(
    extraction: Path, manifest: dict[str, Any], staging: Path, staged_root: Path
) -> list[dict[str, Any]]:
    """Stage the release assets: CLI binaries with their sidecars, the canonical
    installer assets, FastDB license evidence, and the manifest."""
    staging.mkdir(parents=True, exist_ok=True)
    assets: list[dict[str, Any]] = []

    def stage(name: str, with_sidecar: bool = False) -> None:
        source = extraction / name
        sidecar = extraction / _sidecar_name(name)
        _require(source.is_file(), f"missing staged file {name}")
        if with_sidecar:
            _require(sidecar.is_file(), f"missing sidecar for staged file {name}")
        target = staging / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(source.read_bytes())
        assets.append({
            "name": name,
            "staged_path": target.relative_to(staged_root).as_posix(),
            "sha256": sha256_file(target),
            "bytes": target.stat().st_size,
        })
        if with_sidecar:
            published_sidecar = staging / _sidecar_name(name)
            published_sidecar.write_bytes(sidecar.read_bytes())
            assets.append({
                "name": _sidecar_name(name),
                "staged_path": published_sidecar.relative_to(staged_root).as_posix(),
                "sha256": sha256_file(published_sidecar),
                "bytes": published_sidecar.stat().st_size,
            })

    for entry in manifest["cli"]:
        stage(entry["name"], with_sidecar=True)
    for name in EXPECTED_INSTALLER_NAMES:
        stage(name)
    for license_name in FASTDB_LICENSE_ASSETS:
        stage(license_name)
    stage(MANIFEST_NAME)
    return assets


def stage_pypi_files(
    extraction: Path, manifest: dict[str, Any], staging: Path, staged_root: Path
) -> list[dict[str, Any]]:
    """Stage only c_two wheels and the sdist; FastDB proof inputs are never staged."""
    staging.mkdir(parents=True, exist_ok=True)
    files: list[dict[str, Any]] = []
    for entry in [*manifest["wheels"], *manifest["sdist"]]:
        source = extraction / entry["name"]
        _require(source.is_file(), f"missing dist file {entry['name']}")
        _require(
            entry["name"].startswith("c_two-") and not entry["name"].startswith("fastdb4py-"),
            f"refusing to publish {entry['name']} as a C-Two package",
        )
        target = staging / entry["name"]
        target.write_bytes(source.read_bytes())
        files.append({"name": entry["name"], "staged_path": target.relative_to(staged_root).as_posix(),
                      "sha256": sha256_file(target), "bytes": target.stat().st_size})
    _require(len(files) == 36, f"expected 36 PyPI files, staged {len(files)}")
    return files


def resolve_tag_commit(api: GitHubApi, tag: str) -> str | None:
    status, reference = api.get_json(f"/repos/{api.repository}/git/ref/tags/{tag}")
    if status == 404:
        return None
    _require(isinstance(reference, dict), f"malformed tag reference for {tag}")
    obj = reference.get("object")
    _require(isinstance(obj, dict), f"tag {tag} reference has no object")
    if obj.get("type") == "commit":
        return obj.get("sha")
    if obj.get("type") == "tag":
        status, annotated = api.get_json(f"/repos/{api.repository}/git/tags/{obj.get('sha')}")
        _require(status == 200 and isinstance(annotated, dict), f"cannot resolve annotated tag {tag}")
        inner = annotated.get("object")
        _require(isinstance(inner, dict) and inner.get("type") == "commit",
                 f"annotated tag {tag} does not point at a commit")
        return inner.get("sha")
    raise PromotionError(f"tag {tag} points at unsupported object type {obj.get('type')!r}")


def github_release_state(
    api: GitHubApi, tag: str, title: str, source_sha: str, assets: list[dict[str, Any]],
) -> dict[str, Any]:
    """Classify each planned asset against the existing release; never clobber or move tags."""
    tag_commit = resolve_tag_commit(api, tag)
    if tag_commit is not None and tag_commit != source_sha:
        raise PromotionError(
            f"tag {tag} already points at {tag_commit}, not the candidate source {source_sha}; "
            "refusing to move an existing tag"
        )
    status, release = api.get_json(f"/repos/{api.repository}/releases/tags/{tag}")
    release_exists = status == 200
    if release_exists:
        _require(isinstance(release, dict), f"malformed release for {tag}")
        _require(release.get("name") == title,
                 f"existing release for {tag} is named {release.get('name')!r}, expected {title!r}")
        remote_assets = {
            asset.get("name"): asset
            for asset in release.get("assets", [])
            if isinstance(asset, dict)
        }
        planned_names = {asset["name"] for asset in assets}
        foreign = sorted(set(remote_assets) - planned_names)
        _require(not foreign, f"release {tag} carries foreign assets {foreign}; refusing to touch them")
        for asset in assets:
            remote = remote_assets.get(asset["name"])
            if remote is None:
                asset["disposition"] = "upload"
                continue
            status, blob = api.get_bytes(
                f"/repos/{api.repository}/releases/assets/{remote.get('id')}",
                accept="application/octet-stream",
            )
            _require(status == 200,
                     f"cannot download existing asset {asset['name']} for byte comparison")
            if sha256_bytes(blob) == asset["sha256"]:
                asset["disposition"] = "verified-existing"
            else:
                raise PromotionError(
                    f"release {tag} asset {asset['name']} already exists with different bytes; "
                    "refusing to clobber"
                )
    else:
        for asset in assets:
            asset["disposition"] = "upload"
    to_upload = [asset["name"] for asset in assets if asset["disposition"] == "upload"]
    return {
        "tag": tag,
        "title": title,
        "tag_exists": tag_commit is not None,
        "tag_commit": tag_commit,
        "release_exists": release_exists,
        "action": "none" if release_exists and not to_upload else "upload",
        "assets": assets,
    }


def _pypi_version_listing(project: str, version: str) -> dict[str, str] | None:
    """Fetch the published filename -> sha256 listing for one version (None on 404)."""
    url = f"{PYPI_HOST}/pypi/{project}/{version}/json"
    request = urllib.request.Request(url, headers={"User-Agent": "c-two-release-promotion"})
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            payload = json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise PromotionError(f"PyPI check for {project} {version} returned HTTP {error.code}") from error
    except (urllib.error.URLError, OSError, UnicodeError, json.JSONDecodeError) as error:
        raise PromotionError(f"PyPI check for {project} {version} failed: {error}") from error
    published: dict[str, str] = {}
    for entry in payload.get("urls", []):
        if isinstance(entry, dict) and isinstance(entry.get("filename"), str):
            digests = entry.get("digests")
            sha256 = digests.get("sha256") if isinstance(digests, dict) else None
            _require(
                isinstance(sha256, str) and re.fullmatch(r"[0-9a-f]{64}", sha256),
                f"PyPI file {entry['filename']} has no valid sha256 digest",
            )
            published[entry["filename"]] = sha256
    return published


def pypi_state(
    project: str, version: str, files: list[dict[str, Any]],
) -> dict[str, Any]:
    """Compare the staged dist files with the registry; only byte-equal files may be skipped."""
    published = _pypi_version_listing(project, version)
    if published is None:
        for entry in files:
            entry["disposition"] = "upload"
        return {"project": project, "version": version, "action": "upload", "files": files}
    planned_names = {entry["name"] for entry in files}
    foreign = sorted(set(published) - planned_names)
    _require(not foreign,
             f"PyPI {project} {version} already publishes foreign files {foreign}")
    for entry in files:
        remote = published.get(entry["name"])
        if remote is None:
            entry["disposition"] = "upload"
        elif remote == entry["sha256"]:
            entry["disposition"] = "verified-existing"
        else:
            entry["disposition"] = "conflict"
    conflicts = [entry for entry in files if entry["disposition"] == "conflict"]
    if conflicts:
        # A published PyPI version is immutable: files the registry already
        # holds under different bytes can never be completed from this
        # candidate, so the collision always fails closed.
        first = conflicts[0]
        raise PromotionError(
            f"PyPI {project} {version} already publishes {first['name']} with different "
            f"bytes (pypi {published[first['name']]}, candidate {first['sha256']}); "
            "refusing to clobber"
        )
    to_upload = [entry["name"] for entry in files if entry["disposition"] == "upload"]
    return {"project": project, "version": version,
            "action": "upload" if to_upload else "none", "files": files}


def write_release_body(path: Path, plan: dict[str, Any], manifest_sha: str) -> None:
    versions = plan["versions"]
    candidate = plan["candidate_run"]
    lines = [
        f"# {plan['github_release']['title']}",
        "",
        "Promoted from the exact tested release-candidate bytes; nothing was rebuilt for this release.",
        "",
        f"- c3 version: `{versions['c3_version']}` (Python package `c-two=={versions['c_two_version']}`)",
        f"- Source commit: `{plan['source_sha']}` (Release Candidate dispatch on canonical `main`)",
        f"- Native FastDB Core: `{SOURCE_VALIDATION_FASTDB_CORE_VERSION}`, statically linked "
        f"from source `{versions['fastdb_source_rev']}`",
        f"- Optional Python FastDB: `fastdb4py=={versions['fastdb_version']}`; "
        "its upstream LICENSE and THIRD_PARTY_NOTICES are attached verbatim",
        f"- Release-candidate manifest: `rc-manifest.json` (SHA-256 `{manifest_sha}`)",
        "",
        "Validation provenance:",
        "",
        f"- Release Candidate run: {candidate['url']}",
        f"- Source Validation bound run: "
        f"{next(iter(plan['source_validation'].values()))['run_url']}",
    ]
    for label, entry in sorted(plan["source_validation"].items()):
        lines.append(f"- Source Validation full-scope run ({label}, py3.12, {entry['gates_passed']} gates): "
                     f"{entry['run_url']}")
    lines += [
        "",
        "Validated coverage: 35 executed Python ABI rows (6 platforms, with Python 3.10–3.14 "
        "and free-threaded cp314-cp314t except Windows ARM64 starts at 3.11), "
        "6 installed-wheel lifecycle smokes, "
        "4 Windows standard-user lifecycle/cleanup receipts, an isolated auditwheel-repaired "
        "sdist build/import/lifecycle proof, and the 18-row Rust/Python portable matrix plus "
        "12-row TypeScript real-call receipt on Ubuntu 24.04, macOS 15, Windows Server 2022/2025 "
        "and Windows 11 ARM64 CI runners "
        "(the Source Validation gates exercise each lane's own debug c3 build; the released executables "
        "are the exact tested release candidates).",
        "",
    ]
    path.write_text("\n".join(lines), encoding="utf-8")


def command_prepare(options: argparse.Namespace) -> int:
    repository = os.environ.get("GITHUB_REPOSITORY", "")
    if repository != options.expect_repository:
        print(
            f"error: refusing to promote from repository {repository!r}; "
            f"promotion is only valid on {options.expect_repository}",
            file=sys.stderr,
        )
        return 1
    # Promotion is an explicit canonical-main dispatch; no workflow_run
    # listener reaches this code anymore.
    ref = os.environ.get("GITHUB_REF", "")
    if ref != EXPECT_REF:
        print(
            f"error: refusing to promote from ref {ref!r}; only {EXPECT_REF} may publish",
            file=sys.stderr,
        )
        return 1
    token = os.environ.get("GITHUB_TOKEN", "")
    if not token:
        print("error: GITHUB_TOKEN is required for authenticated artifact provenance", file=sys.stderr)
        return 1
    api = GitHubApi(
        os.environ.get("GITHUB_API_URL", "https://api.github.com"), repository, token
    )

    candidate_run_id = options.candidate_run_id
    status, run = api.get_json(f"/repos/{repository}/actions/runs/{candidate_run_id}")
    _require(status == 200 and isinstance(run, dict),
             f"candidate run {candidate_run_id} not found")
    run_sha = run.get("head_sha")
    _require(SHA_PATTERN.match(run_sha or ""), "candidate run head_sha is malformed")
    if options.source_sha:
        _require(options.source_sha == run_sha,
                 f"candidate run {candidate_run_id} source {run_sha} != requested {options.source_sha}")
    source_sha = run_sha
    validate_gate_run(api, run, CANDIDATE_WORKFLOW_FILE, CANDIDATE_WORKFLOW_NAME,
                      source_sha, (CANDIDATE_EVENT,))
    require_gate_success(CANDIDATE_WORKFLOW_NAME, run)
    candidate = run

    require_passed_jobs(api, candidate["id"], {"context", "manifest"})

    download_dir = Path(options.download_dir)
    extraction = download_dir / "candidate"
    extraction.mkdir(parents=True, exist_ok=True)
    seen_members: dict[str, tuple[str, str]] = {}
    for artifact in api.run_artifacts(candidate["id"]):
        # Match release-candidate.yml's rc-* collection, excluding diagnostics.
        if artifact["name"].startswith("rc-"):
            download_and_extract_artifact(api, artifact, extraction, seen_members)

    verify_sidecars(extraction)
    manifest = _load_json_file(extraction / MANIFEST_NAME)
    _require(isinstance(manifest, dict), "rc-manifest.json is not an object")
    versions, source_run_id = verify_manifest_binding(manifest, extraction, source_sha)
    manifest_sha = sha256_file(extraction / MANIFEST_NAME)
    wheel_by_row = verify_wheel_rows(manifest["wheels"], versions["c_two_version"])
    verify_candidate_receipts(extraction, manifest, source_sha, wheel_by_row)

    # The Source Validation evidence comes only from the run the candidate
    # context bound by identity; a different run for the same source never
    # substitutes it and receipts are never mixed across runs.
    status, source_run = api.get_json(f"/repos/{repository}/actions/runs/{source_run_id}")
    _require(status == 200 and isinstance(source_run, dict),
             f"Source Validation run {source_run_id} bound by rc-context.json not found")
    validate_gate_run(api, source_run, SOURCE_VALIDATION_WORKFLOW_FILE,
                      SOURCE_VALIDATION_WORKFLOW_NAME, source_sha, SOURCE_GATE_EVENTS)
    require_gate_success(SOURCE_VALIDATION_WORKFLOW_NAME, source_run)
    source_validation = verify_source_validation(api, source_sha, source_run, download_dir)

    staging = download_dir / "staging"
    plan: dict[str, Any] = {
        "schema": PLAN_SCHEMA,
        "target": options.target,
        "repository": repository,
        "dry_run": bool(options.dry_run),
        "source_sha": source_sha,
        "versions": versions,
        "candidate_run": {"id": candidate.get("id"), "url": candidate.get("html_url")},
        "source_validation": source_validation,
        "manifest_sha256": manifest_sha,
    }
    if options.target == "github-release":
        tag = f"{TAG_PREFIX}{versions['c3_version']}"
        title = f"c3 {versions['c3_version']}"
        assets = stage_github_release_assets(extraction, manifest, staging, download_dir)
        # Read-only remote comparison runs identically in dry runs; only the
        # caller's publishing steps are suppressed.
        plan["github_release"] = github_release_state(api, tag, title, source_sha, assets)
    else:
        files = stage_pypi_files(extraction, manifest, staging / "dist", download_dir)
        plan["pypi"] = pypi_state(options.pypi_project, versions["c_two_version"], files)
        # The publishing action consumes the staging directory directly, so it
        # must hold exactly the registry-missing files; verified-existing
        # copies are removed from staging, never from the extracted sources.
        for entry in plan["pypi"]["files"]:
            if entry["disposition"] == "verified-existing":
                staged_file = download_dir / entry["staged_path"]
                if staged_file.is_file():
                    staged_file.unlink()
        staged = sorted(path.name for path in (staging / "dist").iterdir())
        expected = sorted(entry["name"] for entry in plan["pypi"]["files"]
                          if entry["disposition"] == "upload")
        _require(staged == expected,
                 f"PyPI staging holds {staged}, expected exactly the missing files {expected}")

    Path(options.plan_output).write_text(
        json.dumps(plan, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    if options.release_body_output and options.target == "github-release":
        write_release_body(Path(options.release_body_output), plan, manifest_sha)
    summary = plan.get("github_release") or plan.get("pypi")
    print(
        f"promotion plan {options.plan_output}: target={options.target} "
        f"source={source_sha[:12]} candidate_run={candidate.get('id')} action={summary['action']}"
        + (" [dry run]" if options.dry_run else "")
    )
    return 0


def command_check_source(options: argparse.Namespace) -> int:
    repository = os.environ.get("GITHUB_REPOSITORY", "")
    if repository != options.expect_repository:
        print(
            f"error: refusing to check from repository {repository!r}; "
            f"source checks are only valid on {options.expect_repository}",
            file=sys.stderr,
        )
        return 1
    ref = os.environ.get("GITHUB_REF", "")
    if ref != EXPECT_REF:
        print(
            f"error: refusing to check from ref {ref!r}; only {EXPECT_REF} names canonical main",
            file=sys.stderr,
        )
        return 1
    token = os.environ.get("GITHUB_TOKEN", "")
    if not token:
        print("error: GITHUB_TOKEN is required for authenticated run provenance", file=sys.stderr)
        return 1
    source_sha = options.source_sha
    _require(bool(SHA_PATTERN.match(source_sha)), f"source SHA {source_sha!r} is malformed")

    api = GitHubApi(
        os.environ.get("GITHUB_API_URL", "https://api.github.com"), repository, token
    )
    run = find_full_source_validation_run(api, source_sha)
    Path(options.plan_output).write_text(
        json.dumps(
            {"source_sha": source_sha, "run_id": run.get("id"), "run_url": run.get("html_url")},
            indent=2, sort_keys=True,
        ) + "\n",
        encoding="utf-8",
    )
    print(
        f"source check {options.plan_output}: source={source_sha[:12]} "
        f"source_validation_run={run.get('id')} ({run.get('event')})"
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    prepare = subparsers.add_parser(
        "prepare", help="Download, verify, and stage an exact-tested-candidate promotion"
    )
    prepare.add_argument("--target", choices=("github-release", "pypi"), required=True)
    prepare.add_argument("--candidate-run-id", type=int, required=True,
                         help="The exact successful Release Candidate workflow_dispatch run "
                              "on canonical main whose retained bytes are promoted")
    prepare.add_argument("--source-sha", default=None,
                         help="Optional cross-check; must equal the candidate run's head SHA")
    prepare.add_argument("--expect-repository", default=EXPECT_REPOSITORY)
    prepare.add_argument("--download-dir", default="candidate-download")
    prepare.add_argument("--plan-output", default="promotion-plan.json")
    prepare.add_argument("--release-body-output", default=None)
    prepare.add_argument("--pypi-project", default="c-two")
    prepare.add_argument("--dry-run", action="store_true",
                         help="Identical read-only verification including remote comparison; "
                              "the caller still performs no publishing")
    prepare.set_defaults(handler=command_prepare)
    check_source = subparsers.add_parser(
        "check-source",
        help="Read-only: resolve the successful full Source Validation run for one source SHA "
             "using only GitHub metadata (no artifact downloads)",
    )
    check_source.add_argument("--source-sha", required=True)
    check_source.add_argument("--expect-repository", default=EXPECT_REPOSITORY)
    check_source.add_argument("--plan-output", default="source-check.json")
    check_source.set_defaults(handler=command_check_source)
    options = parser.parse_args(argv)
    try:
        return options.handler(options)
    except PromotionError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
