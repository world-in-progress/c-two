"""Byte-exact candidate promotion and environment-protected publish contracts.

Network-free fixtures exercise candidate bytes, receipts and remote state;
source proof is selected using shared tree-bound metadata verification.
"""

from __future__ import annotations

import hashlib
import io
import json
import os
import re
import stat
import subprocess
import sys
import urllib.error
import urllib.request
import zipfile
from email.message import Message
from pathlib import Path
from typing import Any

import pytest

from posix_shell import posix_shell, shell_environment

_ROOT = next(
    parent
    for parent in Path(__file__).resolve().parents
    if (parent / ".github" / "scripts" / "promote_release_candidate.py").is_file()
)

import importlib.util

_spec = importlib.util.spec_from_file_location(
    "promote_release_candidate",
    _ROOT / ".github" / "scripts" / "promote_release_candidate.py",
)
assert _spec is not None and _spec.loader is not None
promote = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(promote)

from source_evidence import FULL_LANES

REPOSITORY = "world-in-progress/c-two"
SOURCE_SHA = "a" * 40
OTHER_SHA = "b" * 40
C_TWO_VERSION = "0.6.0"
C3_VERSION = "0.2.0"
FASTDB_VERSION = "0.2.4"
FASTDB_CORE_VERSION = "0.2.4"
FASTDB_SOURCE_REV = promote.FASTDB_SOURCE_REV
CLI_TARGETS = promote.CLI_TARGETS
WINDOWS_TARGET = "x86_64-pc-windows-msvc"
WINDOWS_ARM64_TARGET = "aarch64-pc-windows-msvc"
WINDOWS_TARGETS = (WINDOWS_TARGET, WINDOWS_ARM64_TARGET)
PYTHONS = promote.EXPECTED_PYTHONS
PYTHONS_BY_TARGET = {target: PYTHONS[1:] if target == WINDOWS_ARM64_TARGET else PYTHONS
                     for target in CLI_TARGETS}
PLATFORM_TAGS = {
    "x86_64-unknown-linux-gnu": "manylinux_2_17_x86_64.manylinux2014_x86_64",
    "aarch64-unknown-linux-gnu": "manylinux_2_17_aarch64.manylinux2014_aarch64",
    "aarch64-apple-darwin": "macosx_11_0_arm64",
    "x86_64-apple-darwin": "macosx_11_0_x86_64",
    "x86_64-pc-windows-msvc": "win_amd64",
    "aarch64-pc-windows-msvc": "win_arm64",
}
# Targets whose FastDB dependency is built from the published sdist; the
# other targets consume published wheels directly.
SDIST_MODE_TARGETS = ("aarch64-unknown-linux-gnu", "x86_64-apple-darwin")


def _sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _digest(character: str) -> str:
    return character * 64


def _run(run_id: int, name: str, sha: str = SOURCE_SHA, **overrides: Any) -> dict[str, Any]:
    run = {
        "id": run_id,
        "name": name,
        # Candidates exist only as explicit main dispatches; source gates are
        # canonical main pushes by default (dispatch variants are built per test).
        "event": "workflow_dispatch" if name == "Release Candidate" else "push",
        "head_branch": "main",
        "head_sha": sha,
        "status": "completed",
        "conclusion": "success",
        "run_number": run_id,
        "path": ".github/workflows/" + ("release-candidate.yml" if name == "Release Candidate" else "ci.yml"),
        "repository": {"full_name": REPOSITORY},
        "head_repository": {"full_name": REPOSITORY},
        "html_url": f"https://github.com/{REPOSITORY}/actions/runs/{run_id}",
    }
    if name == "Source Validation":
        run["runner_labels"] = list(FULL_LANES)
    run.update(overrides)
    return run


def _cli_name(target: str) -> str:
    return f"c3-{target}{'.exe' if target in WINDOWS_TARGETS else ''}"


def _wheel_name(target: str, python: str) -> str:
    interpreter, abi = promote.PYTHON_ABI_PAIRS[python]
    return f"c_two-{C_TWO_VERSION}-{interpreter}-{abi}-{PLATFORM_TAGS[target]}.whl"


def _fastdb_wheel_name(target: str, python: str) -> str:
    interpreter, abi = promote.PYTHON_ABI_PAIRS[python]
    # The macOS Intel sdist build retains this tag; its receipt proves that target.
    platform = "macosx_15_0_universal2" if target == "x86_64-apple-darwin" else PLATFORM_TAGS[target]
    return f"fastdb4py-{FASTDB_VERSION}-{interpreter}-{abi}-{platform}.whl"


def _wheel_entry(name: str, data: bytes, distribution: str, version: str) -> dict[str, Any]:
    return {"name": name, "sha256": _sha(data), "bytes": len(data),
            "distribution": distribution, "version": version}


def _smoke_body(
    *, artifacts: list[dict[str, Any]], administrator: bool | None = None,
) -> dict[str, Any]:
    """A realistic installed-wheel-smoke receipt body (mirrors the runner output)."""
    installed = {
        entry["distribution"]: {
            "direct_url": {"url": f"file:///tmp/{entry['name']}", "archive_info": {}},
            "sha256": entry["sha256"],
        }
        for entry in artifacts if entry.get("distribution")
    }
    check = {
        "paths": {"c_two": "/tmp/venv/lib/c_two/__init__.py"},
        "prefix": "/tmp/venv",
        "pid": 111,
        "installed_wheels": installed,
        "versions": {"c-two": C_TWO_VERSION, "fastdb4py": FASTDB_VERSION, "numpy": "2.5.3"},
        "status": "passed",
        "transport": "direct",
        "observed_mode": "ipc",
        "payload_bytes": 1048576,
        "held_view_invalidated": True,
    }
    relay = dict(check, pid=112, transport="relay", observed_mode="http")
    windows_platform = "win-arm64" if any("win_arm64" in item["name"] for item in artifacts) else "win-amd64"
    machine = 0xAA64 if windows_platform == "win-arm64" else 0x8664
    return {
        "schema": "c-two.installed-wheel-smoke.v1",
        "status": "passed",
        "source_commit_sha": SOURCE_SHA,
        "platform": "TestPlatform-26.1-test",
        "interpreter": "3.12.3 (main, test)",
        "checks": [check, relay],
        "base_install": {"status": "passed", "transports": ["thread", "ipc"],
                         "optional_dependencies_present": False, "optional_modules_loaded": False},
        "cleanup": {"host_orderly_shutdown": True, "errors": [], "relay_stop": "terminated",
                    "host_stop": "already_exited", "processes_exited": True,
                    "temporary_directory_removed": True},
        "identity": {"administrator": administrator},
        "artifacts": artifacts,
        "native_architecture": {"python_platform": windows_platform,
                                "pe_machines": {item["name"]: machine for item in artifacts}},
        "installed_packages": [
            {"name": "c-two", "version": C_TWO_VERSION},
            {"name": "fastdb4py", "version": FASTDB_VERSION},
            {"name": "numpy", "version": "2.5.3"},
        ],
        "cli_version": f"c3 {C3_VERSION}",
        "host": {"paths": {}, "prefix": "/tmp/venv", "pid": 110, "status": "ready"},
        "borrowed_inputs_invalidated": 4,
        "logs": {"host.log": "", "relay.log": ""},
    }


class CandidateBuilder:
    """Builds a structurally exact synthetic candidate plus its manifest."""

    def __init__(self) -> None:
        self.members: dict[str, bytes] = {}
        self.cli: list[dict[str, Any]] = []
        self.wheels: list[dict[str, Any]] = []
        self.sdist: list[dict[str, Any]] = []
        self.fastdb_proof: list[dict[str, Any]] = []
        self.installers: list[dict[str, Any]] = []
        self.receipts: list[dict[str, Any]] = {}
        self.receipt_order: list[dict[str, Any]] = []
        self.wheel_by_row: dict[str, str] = {}
        self.published_listing: dict[str, str] = {}
        self.cli_by_name: dict[str, dict[str, Any]] = {}
        self.wheel_data: dict[str, bytes] = {}
        self._build()

    def _put(self, name: str, data: bytes) -> dict[str, Any]:
        self.members[name] = data
        self.members[f"{name}.sha256"] = f"{_sha(data)}  {name}\n".encode()
        return {"name": name, "bytes": len(data), "sha256": _sha(data)}

    def _put_json(self, name: str, payload: dict[str, Any]) -> dict[str, Any]:
        return self._put(name, json.dumps(payload, indent=2, sort_keys=True).encode() + b"\n")

    def _rewrite_json(self, name: str, payload: dict[str, Any]) -> None:
        entry = self._put_json(name, payload)
        self.receipts[name] = entry

    def _record_receipt(self, name: str, payload: dict[str, Any]) -> None:
        self.receipts[name] = self._put_json(name, payload)

    def _build(self) -> None:
        for target in CLI_TARGETS:
            entry = self._put(_cli_name(target), f"cli-bytes-{target}".encode())
            self.cli.append(entry)
            self.cli_by_name[entry["name"]] = entry
        for target in CLI_TARGETS:
            for python in PYTHONS_BY_TARGET[target]:
                name = _wheel_name(target, python)
                data = f"wheel-bytes-{target}-{python}".encode()
                self.wheel_data[name] = data
                self.wheels.append(self._put(name, data))
                self.wheel_by_row[f"{target}/{python}"] = name
        self.sdist.append(self._put(f"c_two-{C_TWO_VERSION}.tar.gz", b"sdist-bytes"))

        for name, data in (
            ("fastdb-LICENSE", b"fastdb upstream license\n"),
            ("fastdb-THIRD_PARTY_NOTICES.txt", b"fastdb upstream notices\n"),
        ):
            self.fastdb_proof.append(self._put(name, data))
        self.fastdb_sdist_name = f"fastdb4py-{FASTDB_VERSION}.tar.gz"
        self.fastdb_sdist_data = b"fastdb-sdist-bytes"
        self.fastdb_proof.append(self._put(self.fastdb_sdist_name, self.fastdb_sdist_data))
        self.published_listing[self.fastdb_sdist_name] = _sha(self.fastdb_sdist_data)

        # Retained proof wheels: the published cp312 wheel for wheel-mode
        # targets and the cp312 wheel built from the verified sdist for the
        # sdist-mode targets. Their digests feed the row decisions below.
        self.retained_fastdb: dict[str, dict[str, Any]] = {}
        for target in CLI_TARGETS:
            name = _fastdb_wheel_name(target, "3.12")
            data = f"fastdb-wheel-{target}".encode()
            self.fastdb_proof.append(self._put(name, data))
            self.retained_fastdb[target] = {"name": name, "sha256": _sha(data),
                                            "bytes": len(data)}
            if target not in SDIST_MODE_TARGETS:
                self.published_listing[name] = _sha(data)
        # Wheel-mode rows consume published wheels for every interpreter; the
        # cp312 entry keeps the retained proof wheel's digest.
        for target in CLI_TARGETS:
            if target in SDIST_MODE_TARGETS:
                continue
            for python in PYTHONS_BY_TARGET[target]:
                if python == "3.12":
                    continue
                name = _fastdb_wheel_name(target, python)
                self.published_listing[name] = _sha(f"published-{target}-{python}".encode())

        for name, data in (
            ("c3-installer.sh", b"#!/bin/sh\n# unix installer\n"),
            ("c3-installer.ps1", b"# windows installer\n"),
        ):
            self.installers.append(self._put(name, data))

        self._record_receipt("rc-context.json", {
            "schema": "c-two.release-candidate.context.v2",
            "source_commit_sha": SOURCE_SHA,
            "event": "workflow_dispatch",
            "ref": "refs/heads/main",
            "pull_request_head_sha": None,
            "c_two_version": C_TWO_VERSION,
            "c3_version": C3_VERSION,
            "fastdb_version": FASTDB_VERSION,
            "fastdb_source_rev": FASTDB_SOURCE_REV,
        })
        self._record_receipt("rc-fastdb-public.json", {
            "files": [
                {"filename": name, "sha256": sha,
                 "packagetype": "sdist" if name.endswith(".tar.gz") else "bdist_wheel"}
                for name, sha in sorted(self.published_listing.items())
            ],
        })
        base = {"schema": "c-two.fastdb-release.resolution.v1",
                "project": "fastdb4py", "version": FASTDB_VERSION}
        self._record_receipt("fastdb-sdist.json", {
            **base, "mode": "sdist", "filename": self.fastdb_sdist_name,
            "sha256": _sha(self.fastdb_sdist_data)})
        for target in CLI_TARGETS:
            retained = self.retained_fastdb[target]
            if target in SDIST_MODE_TARGETS:
                decision = {**base, "mode": "sdist", "filename": self.fastdb_sdist_name,
                            "sha256": _sha(self.fastdb_sdist_data),
                            "built_wheel": {**retained, "derived_from_sdist_sha256": _sha(self.fastdb_sdist_data)}}
            else:
                decision = {**base, "mode": "wheel", "filename": retained["name"], "sha256": retained["sha256"]}
            self._record_receipt(f"fastdb-{target}.json", decision)

        # 35 executed ABI rows: installed-wheel-smoke receipts bound to rows.
        for target in CLI_TARGETS:
            for python in PYTHONS_BY_TARGET[target]:
                cli = self.cli_by_name[_cli_name(target)]
                ctwo_name = self.wheel_by_row[f"{target}/{python}"]
                ctwo_data = self.wheel_data[ctwo_name]
                if target in SDIST_MODE_TARGETS:
                    built_name = _fastdb_wheel_name(target, python)
                    # Independent per-ABI builds need not reproduce the
                    # provisioning job's cp312 wheel bytes.
                    built_data = f"built-{target}-{python}".encode()
                    built = {"name": built_name, "sha256": _sha(built_data),
                             "bytes": len(built_data)}
                    fastdb_artifact = {
                        "name": built["name"], "sha256": built["sha256"],
                        "bytes": built["bytes"], "distribution": "fastdb4py",
                        "version": FASTDB_VERSION,
                    }
                    decision = {
                        "mode": "sdist",
                        "sha256": _sha(self.fastdb_sdist_data),
                        "built_wheel": {
                            "name": built["name"], "sha256": built["sha256"],
                            "bytes": built["bytes"],
                            "derived_from_sdist_sha256": _sha(self.fastdb_sdist_data),
                        },
                    }
                else:
                    wheel_name = _fastdb_wheel_name(target, python)
                    wheel_sha = (self.retained_fastdb[target]["sha256"] if python == "3.12"
                                 else self.published_listing[wheel_name])
                    fastdb_artifact = {
                        "name": wheel_name, "sha256": wheel_sha, "bytes": 4096,
                        "distribution": "fastdb4py", "version": FASTDB_VERSION,
                    }
                    decision = {"mode": "wheel", "filename": wheel_name, "sha256": wheel_sha}
                interpreter, abi = promote.PYTHON_ABI_PAIRS[python]
                receipt = _smoke_body(artifacts=[
                    fastdb_artifact,
                    _wheel_entry(ctwo_name, ctwo_data, "c-two", C_TWO_VERSION),
                    {"name": cli["name"], "sha256": cli["sha256"], "bytes": cli["bytes"]},
                ])
                receipt["row"] = {"target": target, "python": python,
                                  "fastdb_mode": decision["mode"]}
                receipt["wheel_abi"] = {"interpreter": interpreter, "abi": abi}
                receipt["fastdb"] = decision
                self._record_receipt(f"abi-{target}-{python}.json", receipt)

        # 6 lifecycle smokes on the cp312 wheel of each target (no decision
        # binding: the plain smoke helper records only the consumed entries).
        for target in CLI_TARGETS:
            cli = self.cli_by_name[_cli_name(target)]
            ctwo_name = self.wheel_by_row[f"{target}/3.12"]
            retained = self.retained_fastdb[target]
            fastdb_artifact = {"name": retained["name"], "sha256": retained["sha256"],
                               "bytes": retained["bytes"], "distribution": "fastdb4py",
                               "version": FASTDB_VERSION}
            self._record_receipt(f"smoke-{target}.json", _smoke_body(artifacts=[
                fastdb_artifact,
                _wheel_entry(ctwo_name, self.wheel_data[ctwo_name], "c-two", C_TWO_VERSION),
                {"name": cli["name"], "sha256": cli["sha256"], "bytes": cli["bytes"]},
            ]))

        # Windows standard-user lifecycle/cleanup, ordinary + wrapper.
        for target in WINDOWS_TARGETS:
            windows_cli = self.cli_by_name[_cli_name(target)]
            windows_ctwo = self.wheel_by_row[f"{target}/3.12"]
            retained = self.retained_fastdb[target]
            standard = _smoke_body(
                administrator=False,
                artifacts=[
                    {"name": retained["name"], "sha256": retained["sha256"],
                     "bytes": retained["bytes"], "distribution": "fastdb4py",
                     "version": FASTDB_VERSION},
                    _wheel_entry(windows_ctwo, self.wheel_data[windows_ctwo], "c-two", C_TWO_VERSION),
                    {"name": windows_cli["name"], "sha256": windows_cli["sha256"],
                     "bytes": windows_cli["bytes"]},
                ])
            self._record_receipt(f"standard-user-{target}.json", standard)
            self._record_receipt(f"standard-user-{target}.json.wrapper.json", {
                "schema": "c-two.standard-user-wrapper.v1",
                "status": "passed",
                "errors": [],
                "cleanup": {"processes_exited": True, "workspace_removed": True,
                            "account_removed": True},
                "account_sid": "S-1-5-21-test",
            })

        self._record_receipt("sdist-inspect.json", {
            "schema": "c-two.release-candidate.sdist-inspection.v1",
            "status": "passed",
            "source_commit_sha": SOURCE_SHA,
            "archive": f"c_two-{C_TWO_VERSION}.tar.gz",
            "project": {"name": "c-two", "version": C_TWO_VERSION},
        })
        linux_retained = self.retained_fastdb["x86_64-unknown-linux-gnu"]
        self._record_receipt("sdist-proof.json", {
            "schema": "c-two.release-candidate.sdist-proof.v1",
            "status": "executed",
            "source_commit_sha": SOURCE_SHA,
            "sdist": dict(self.sdist[0]),
            "isolated_wheel": {"name": "c_two-0.6.0-cp312-cp312-manylinux_2_39_x86_64.whl",
                               "sha256": _sha(b"isolated-wheel")},
            "link_mode": "system",
            "sdk": {"version": FASTDB_CORE_VERSION, "source_sha": FASTDB_SOURCE_REV},
            "bundled_fastdb_libs": ["libfastdb.so"],
            "elf_dynamic": {"runpath": "$ORIGIN/../c_two.libs", "needed": ["libfastdb.so"]},
            "standalone_import": "passed",
            "lifecycle": _smoke_body(artifacts=[
                {"name": "c_two-0.6.0-cp312-cp312-manylinux_2_39_x86_64.whl",
                 "sha256": _sha(b"isolated-wheel"), "distribution": "c-two", "version": C_TWO_VERSION},
                {**linux_retained, "distribution": "fastdb4py", "version": FASTDB_VERSION},
                self.cli_by_name["c3-x86_64-unknown-linux-gnu"],
            ]),
            "checks": ["build from extracted contents", "standalone import"],
            "fastdb": {"mode": "wheel", "filename": linux_retained["name"],
                       "sha256": linux_retained["sha256"]},
        })

    def drop_member(self, name: str) -> None:
        self.members.pop(name, None)
        self.members.pop(f"{name}.sha256", None)
        self.cli[:] = [e for e in self.cli if e["name"] != name]
        self.wheels[:] = [e for e in self.wheels if e["name"] != name]
        self.sdist[:] = [e for e in self.sdist if e["name"] != name]
        self.fastdb_proof[:] = [e for e in self.fastdb_proof if e["name"] != name]
        self.installers[:] = [e for e in self.installers if e["name"] != name]
        self.receipts.pop(name, None)

    def manifest(self) -> dict[str, Any]:
        receipts = list(self.receipts.values())
        return {
            "schema": promote.MANIFEST_SCHEMA,
            "source_commit_sha": SOURCE_SHA,
            "event": "workflow_dispatch",
            "ref": "refs/heads/main",
            "meta": {
                "c_two_version": C_TWO_VERSION,
                "c3_version": C3_VERSION,
                "fastdb_version": FASTDB_VERSION,
                "fastdb_source_rev": FASTDB_SOURCE_REV,
                "fastdb_sdist_sha256": _sha(self.fastdb_sdist_data),
                "pr_head_sha": "",
            },
            "counts": {
                "cli": len(self.cli), "wheels": len(self.wheels),
                "sdist": len(self.sdist), "fastdb_proof": len(self.fastdb_proof),
                "installers": len(self.installers),
                "receipts_by_prefix": dict(promote.EXPECTED_RECEIPT_PREFIX_COUNTS),
                "receipts": len(receipts),
            },
            "cli": self.cli,
            "wheels": self.wheels,
            "sdist": self.sdist,
            "fastdb_proof": self.fastdb_proof,
            "installers": self.installers,
            "receipts": receipts,
        }


def _zip_bytes(members: dict[str, bytes]) -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        for name, data in members.items():
            archive.writestr(name, data)
    return buffer.getvalue()


class FakeResponse:
    def __init__(self, status: int, body: bytes) -> None:
        self.status = status
        self._body = body

    def read(self) -> bytes:
        return self._body

    def __enter__(self) -> "FakeResponse":
        return self

    def __exit__(self, *args: Any) -> None:
        return None


class FakeApi(promote.GitHubApi):
    """Canned, network-free API responses recording every read access."""

    def __init__(self, world: "World") -> None:
        super().__init__("https://api.test.invalid", REPOSITORY, "test-token")
        self.world = world
        self.paths: list[str] = []

    def get_json(self, path: str) -> tuple[int, Any]:
        self.paths.append(path)
        world = self.world
        repo = f"/repos/{REPOSITORY}"
        if "/git/commits/" in path:
            return 200, {"tree": {"sha": "c" * 40}}
        if "/commits/" in path and "/pulls?" in path:
            return 200, []
        if "/actions/workflows/" in path and "/runs?" in path:
            workflow_file = path.split("/actions/workflows/", 1)[1].split("/runs", 1)[0]
            params = dict(
                param.split("=", 1)
                for param in path.split("/runs?", 1)[1].split("&"))
            runs = world.runs_by_workflow.get(workflow_file, [])

            def matches(run: dict[str, Any]) -> bool:
                if "event" in params and run.get("event") != params["event"]:
                    return False
                if "branch" in params and run.get("head_branch") != params["branch"]:
                    return False
                if "head_sha" in params and run.get("head_sha") != params["head_sha"]:
                    return False
                return True

            return 200, {"workflow_runs": [dict(run) for run in runs if matches(run)]}
        if "/actions/runs/" in path and "/jobs?" in path:
            run_id = int(path.split("/actions/runs/", 1)[1].split("/jobs", 1)[0])
            run = world.runs_by_id[run_id]
            if run["name"] == "Release Candidate":
                names = ["context", "manifest"]
            else:
                names = [f"source ({label}, py3.12)" for label in run["runner_labels"]]
                if "ubuntu-24.04" in run["runner_labels"]:
                    names.append("source (ubuntu-24.04, py3.14t)")
                names.append("workflow policy")
            jobs = getattr(world, "jobs_by_run", {}).get(run_id, [
                {"name": name, "status": "completed", "conclusion": "success", "run_attempt": 1} for name in names])
            return 200, {"jobs": jobs}
        if "/actions/runs/" in path and "/artifacts" in path:
            run_id = int(path.split("/actions/runs/", 1)[1].split("/artifacts", 1)[0])
            return 200, {"artifacts": [dict(entry) for entry in world.artifacts_by_run.get(run_id, [])]}
        match = re.fullmatch(re.escape(repo) + r"/actions/runs/(\d+)", path)
        if match:
            run = world.runs_by_id.get(int(match.group(1)))
            return (200, dict(run)) if run else (404, None)
        if path.startswith(f"{repo}/git/ref/tags/"):
            reference = world.tag_refs.get(path.rsplit("/", 1)[1])
            return (200, dict(reference)) if reference else (404, None)
        if path.startswith(f"{repo}/git/tags/"):
            annotated = world.annotated_tags.get(path.rsplit("/", 1)[1])
            return (200, dict(annotated)) if annotated else (404, None)
        if path.startswith(f"{repo}/releases/tags/"):
            release = world.releases.get(path.rsplit("/", 1)[1])
            return (200, json.loads(json.dumps(release))) if release else (404, None)
        raise AssertionError(f"unexpected GitHub API JSON path: {path}")

    def get_bytes(self, path: str, accept: str = "application/vnd.github+json") -> tuple[int, bytes]:
        self.paths.append(path)
        repo = f"/repos/{REPOSITORY}"
        if "/actions/artifacts/" in path and path.endswith("/zip"):
            blob = self.world.artifact_zips.get(
                int(path.split("/actions/artifacts/", 1)[1].split("/zip", 1)[0]))
            return (200, blob) if blob is not None else (410, b"expired")
        if path.startswith(f"{repo}/releases/assets/"):
            blob = self.world.release_asset_bytes.get(int(path.rsplit("/", 1)[1]))
            assert blob is not None, "unexpected release asset download"
            return 200, blob
        raise AssertionError(f"unexpected GitHub API byte path: {path}")


class FakeUrlOpen:
    def __init__(self, world: "World | None") -> None:
        self.world = world

    def __call__(self, request: Any, timeout: int = 0) -> FakeResponse:
        url = getattr(request, "full_url", str(request))
        assert url.startswith("https://pypi.org/"), f"unexpected network request: {url}"
        if self.world is None or self.world.pypi is None:
            raise urllib.error.HTTPError(url, 404, "Not Found", None, io.BytesIO(b"{}"))
        return FakeResponse(200, json.dumps(self.world.pypi).encode())


class World:
    """Everything the promotion script reads: runs, artifacts, remote state."""

    def __init__(self, candidate: CandidateBuilder | None = None) -> None:
        self.candidate = candidate or CandidateBuilder()
        candidate_run = _run(101, "Release Candidate")
        self.runs_by_workflow: dict[str, list[dict[str, Any]]] = {
            "release-candidate.yml": [candidate_run],
            "ci.yml": [],
        }
        self.runs_by_id: dict[int, dict[str, Any]] = {101: candidate_run}
        self.artifacts_by_run: dict[int, list[dict[str, Any]]] = {}
        self.artifact_zips: dict[int, bytes] = {}
        self.tag_refs: dict[str, dict[str, Any]] = {}
        self.annotated_tags: dict[str, dict[str, Any]] = {}
        self.releases: dict[str, dict[str, Any]] = {}
        self.release_asset_bytes: dict[int, bytes] = {}
        self.pypi: dict[str, Any] | None = None
        self._add_candidate_artifacts()

    def set_source_runs(self, *runs: dict[str, Any]) -> None:
        self.runs_by_workflow["ci.yml"] = list(runs)
        for run in runs:
            self.runs_by_id[run["id"]] = run
            if run.get("status", "completed") == "completed":
                for label in run["runner_labels"]:
                    self._add_source_validation(run["id"], label)

    def _register_artifact(self, run_id: int, artifact_id: int, name: str,
                           members: dict[str, bytes]) -> None:
        blob = _zip_bytes(members)
        self.artifact_zips[artifact_id] = blob
        self.artifacts_by_run.setdefault(run_id, []).append({
            "id": artifact_id, "name": name, "size_in_bytes": len(blob),
            "digest": f"sha256:{_sha(blob)}", "expired": False,
        })

    def _add_candidate_artifacts(self) -> None:
        builder = self.candidate
        manifest = builder.manifest()
        manifest_bytes = json.dumps(manifest, indent=2, sort_keys=True).encode() + b"\n"
        manifest_members = {
            "rc-manifest.json": manifest_bytes,
            "rc-manifest.json.sha256": f"{_sha(manifest_bytes)}  rc-manifest.json\n".encode(),
        }
        context_names = (
            "rc-context.json", "rc-fastdb-public.json", "fastdb-sdist.json",
            "fastdb-LICENSE", "fastdb-THIRD_PARTY_NOTICES.txt",
            builder.fastdb_sdist_name, "c3-installer.sh", "c3-installer.ps1",
        )
        context_members = {
            name: builder.members[name]
            for name in (*context_names, *(f"{n}.sha256" for n in context_names))
            if name in builder.members
        }
        bulk_members = {
            name: data for name, data in builder.members.items()
            if name not in context_members and name not in manifest_members
        }
        self._register_artifact(101, 8001, "rc-context", context_members)
        self._register_artifact(101, 8002, "rc-bulk", bulk_members)
        self._register_artifact(101, 8003, "rc-receipt", manifest_members)
        self.manifest = manifest

    def replace_manifest(self, manifest: dict[str, Any]) -> None:
        self.manifest = manifest
        manifest_bytes = json.dumps(manifest, indent=2, sort_keys=True).encode() + b"\n"
        members = {
            "rc-manifest.json": manifest_bytes,
            "rc-manifest.json.sha256": f"{_sha(manifest_bytes)}  rc-manifest.json\n".encode(),
        }
        self.artifact_zips[8003] = _zip_bytes(members)
        for entry in self.artifacts_by_run[101]:
            if entry["id"] == 8003:
                entry["digest"] = f"sha256:{_sha(self.artifact_zips[8003])}"

    def _add_source_validation(self, run_id: int, label: str) -> None:
        self.artifacts_by_run.setdefault(run_id, []).append({
            "id": 9000 + run_id * 10 + list(FULL_LANES).index(label),
            "name": f"source-{label}-py3.12-full-{SOURCE_SHA}-a1", "expired": False,
        })


def _default_world() -> World:
    world = World()
    world.set_source_runs(
        _run(201, "Source Validation"),
    )
    return world


def _published_release_inventory(tag_sha: str) -> dict[str, Any]:
    """A realistic published rc-manifest.json plus the exact API asset
    metadata (size, sha256 digest) its bytes bind: the six CLI executables,
    the canonical installers, the FastDB license evidence from fastdb_proof,
    the six canonical sidecars, and the manifest itself."""
    cli = []
    for target in CLI_TARGETS:
        name = _cli_name(target)
        data = f"published-cli-bytes-{target}".encode()
        cli.append({"name": name, "sha256": _sha(data), "bytes": len(data)})
    installers = []
    for name, data in (
        ("c3-installer.sh", b"#!/bin/sh\n# published installer\n"),
        ("c3-installer.ps1", b"# published windows installer\n"),
    ):
        installers.append({"name": name, "sha256": _sha(data), "bytes": len(data)})
    licenses = {
        "fastdb-LICENSE": b"published fastdb license\n",
        "fastdb-THIRD_PARTY_NOTICES.txt": b"published fastdb notices\n",
    }
    manifest = {
        "schema": promote.MANIFEST_SCHEMA,
        "source_commit_sha": tag_sha,
        "event": "push",
        "ref": "refs/heads/main",
        "meta": {
            "c_two_version": C_TWO_VERSION,
            "c3_version": C3_VERSION,
            "fastdb_version": FASTDB_VERSION,
            "fastdb_source_rev": FASTDB_SOURCE_REV,
        },
        "cli": cli,
        "installers": installers,
        "fastdb_proof": [
            {"name": f"fastdb4py-{FASTDB_VERSION}.tar.gz",
             "sha256": _sha(b"published fastdb sdist"), "bytes": 22},
            *[{"name": name, "sha256": _sha(data), "bytes": len(data)}
              for name, data in sorted(licenses.items())],
        ],
    }
    manifest_bytes = json.dumps(manifest, indent=2, sort_keys=True).encode() + b"\n"
    api: dict[str, dict[str, Any]] = {
        promote.MANIFEST_NAME: {
            "size": len(manifest_bytes), "digest": f"sha256:{_sha(manifest_bytes)}"},
    }
    for entry in [*cli, *installers]:
        api[entry["name"]] = {"size": entry["bytes"], "digest": f"sha256:{entry['sha256']}"}
    for name, data in licenses.items():
        api[name] = {"size": len(data), "digest": f"sha256:{_sha(data)}"}
    for entry in cli:
        newline = "\r\n" if entry["name"] == _cli_name(WINDOWS_TARGET) else "\n"
        sidecar = f"{entry['sha256']}  {entry['name']}{newline}".encode()
        api[f"{entry['name']}.sha256"] = {
            "size": len(sidecar), "digest": f"sha256:{_sha(sidecar)}"}
    return {"manifest": manifest, "manifest_bytes": manifest_bytes, "api": api}


def _canonical_release_asset_names() -> set[str]:
    """The canonical asset population of one published c3 release, by name."""
    names = {
        name
        for target in CLI_TARGETS
        for name in (_cli_name(target), f"{_cli_name(target)}.sha256")
    }
    names.update(("c3-installer.sh", "c3-installer.ps1"))
    names.update(("fastdb-LICENSE", "fastdb-THIRD_PARTY_NOTICES.txt"))
    names.add(promote.MANIFEST_NAME)
    return names


def _published_release_world(*, tag_sha: str = OTHER_SHA) -> World:
    """c3 0.2.0 already fully released from another commit: the tag points at
    `tag_sha` and the published release carries exactly the canonical asset
    names, each bound through size and API digest to the release's retained
    rc-manifest.json, which is itself bound to that commit."""
    world = _default_world()
    world.tag_refs["c3-v0.2.0"] = {
        "ref": "refs/tags/c3-v0.2.0", "object": {"sha": tag_sha, "type": "commit"}}
    inventory = _published_release_inventory(tag_sha)
    world.release_asset_bytes[4100] = inventory["manifest_bytes"]
    assets = []
    for index, name in enumerate(sorted(_canonical_release_asset_names())):
        metadata = inventory["api"][name]
        assets.append({
            "id": 4100 if name == promote.MANIFEST_NAME else 4000 + index,
            "name": name, "size": metadata["size"], "digest": metadata["digest"],
        })
    world.releases["c3-v0.2.0"] = {
        "tag_name": "c3-v0.2.0", "name": "c3 0.2.0",
        "draft": False, "prerelease": False, "assets": assets,
    }
    return world


def _published_pypi_world() -> World:
    """c-two 0.6.0 already fully published on PyPI with bytes from another
    source (every digest differs from the candidate's)."""
    world = _default_world()
    urls = [
        {"filename": _wheel_name(target, python), "digests": {"sha256": _digest("e")}}
        for target in CLI_TARGETS
        for python in PYTHONS_BY_TARGET[target]
    ]
    urls.append({"filename": f"c_two-{C_TWO_VERSION}.tar.gz",
                 "digests": {"sha256": _digest("e")}})
    world.pypi = {"urls": urls}
    return world


@pytest.fixture()
def environment(monkeypatch: pytest.MonkeyPatch):
    monkeypatch.setenv("GITHUB_REPOSITORY", REPOSITORY)
    monkeypatch.setenv("GITHUB_REF", "refs/heads/main")
    monkeypatch.setenv("GITHUB_TOKEN", "test-token")

    def install(world: World) -> FakeApi:
        api = FakeApi(world)
        monkeypatch.setattr(promote, "GitHubApi", lambda *args: api)
        monkeypatch.setattr(promote.urllib.request, "urlopen", FakeUrlOpen(world))
        return api

    return install


def run_prepare(
    tmp_path: Path,
    *,
    target: str = "github-release",
    candidate_run_id: int = 101,
    extra: list[str] | None = None,
) -> tuple[int, Path, Path]:
    download_dir = tmp_path / "candidate-download"
    plan = tmp_path / "promotion-plan.json"
    body = tmp_path / "release-body.md"
    argv = ["prepare", "--target", target,
            "--download-dir", str(download_dir), "--plan-output", str(plan)]
    if target == "github-release":
        argv += ["--release-body-output", str(body)]
    argv += ["--candidate-run-id", str(candidate_run_id)]
    argv += extra or []
    return promote.main(argv), plan, body


# ── Authenticated GitHub API redirects ────────────────────────────────────────


class RedirectResponse(io.BytesIO):
    def __init__(self, url: str, status: int, headers: dict[str, str], body: bytes) -> None:
        super().__init__(body)
        self.url = url
        self.status = self.code = status
        self.msg = "redirect" if status == 302 else "response"
        self.headers = Message()
        for name, value in headers.items():
            self.headers[name] = value

    def info(self) -> Message:
        return self.headers

    def geturl(self) -> str:
        return self.url


class RedirectTransport(urllib.request.BaseHandler):
    """Keep urllib's real redirect machinery while replacing network I/O."""

    handler_order = 0

    def __init__(self, reply) -> None:
        self.reply = reply
        self.requests: list[tuple[str, str | None]] = []

    def https_open(self, request: urllib.request.Request) -> RedirectResponse:
        authorization = request.get_header("Authorization")
        self.requests.append((request.full_url, authorization))
        status, headers, body = self.reply(request.full_url, authorization)
        return RedirectResponse(request.full_url, status, headers, body)

    http_open = https_open


def _install_redirect_transport(monkeypatch: pytest.MonkeyPatch, transport: RedirectTransport) -> None:
    build_opener = urllib.request.build_opener
    # Route either urllib entry point through the fake transport without network access.
    monkeypatch.setattr(urllib.request, "urlopen", build_opener(transport).open)
    monkeypatch.setattr(
        urllib.request, "build_opener",
        lambda *handlers: build_opener(transport, *handlers),
    )


def test_github_api_preserves_authentication_on_same_origin_redirect(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    api_url = "https://api.github.com/repos/world-in-progress/c-two/actions/runs/123"
    redirected_url = "https://api.github.com/repos/world-in-progress/c-two/actions/runs/123?next=1"

    def reply(url: str, authorization: str | None):
        if url == api_url:
            return 302, {"Location": redirected_url}, b""
        assert url == redirected_url
        assert authorization == "Bearer test-token"
        return 200, {}, b'{"ok": true}'

    transport = RedirectTransport(reply)
    _install_redirect_transport(monkeypatch, transport)
    api = promote.GitHubApi("https://api.github.com", REPOSITORY, "test-token")

    assert api.get_json(f"/repos/{REPOSITORY}/actions/runs/123") == (200, {"ok": True})
    assert transport.requests == [
        (api_url, "Bearer test-token"),
        (redirected_url, "Bearer test-token"),
    ]


def test_github_artifact_redirect_strips_auth_and_accepts_signed_zip(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    api_url = f"https://api.github.com/repos/{REPOSITORY}/actions/artifacts/123/zip"
    signed_url = "https://productionresultssa11.blob.core.windows.net/results/signed.zip?sig=example"
    blob = _zip_bytes({"smoke.txt": b"signed artifact bytes"})

    def reply(url: str, authorization: str | None):
        if url == api_url:
            return 302, {"Location": signed_url}, b""
        assert url == signed_url
        if authorization is not None:
            return 401, {}, b"InvalidAuthenticationInfo"
        return 200, {}, blob

    transport = RedirectTransport(reply)
    _install_redirect_transport(monkeypatch, transport)
    api = promote.GitHubApi("https://api.github.com", REPOSITORY, "test-token")
    artifact = {"name": "rc-smoke-x86_64-apple-darwin", "id": 123,
                "digest": f"sha256:{_sha(blob)}"}

    assert api.get_bytes(f"/repos/{REPOSITORY}/actions/artifacts/123/zip") == (200, blob)
    promote.download_and_extract_artifact(api, artifact, tmp_path, {})

    assert transport.requests == 2 * [(api_url, "Bearer test-token"), (signed_url, None)]
    assert (tmp_path / "smoke.txt").read_bytes() == b"signed artifact bytes"


def test_github_artifact_redirect_refuses_http_downgrade(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    api_url = f"https://api.github.com/repos/{REPOSITORY}/actions/artifacts/123/zip"
    insecure_url = "http://productionresultssa11.blob.core.windows.net/results/signed.zip"

    def reply(url: str, authorization: str | None):
        assert url == api_url, "downgraded URL must never be requested"
        return 302, {"Location": insecure_url}, b""

    transport = RedirectTransport(reply)
    _install_redirect_transport(monkeypatch, transport)
    api = promote.GitHubApi("https://api.github.com", REPOSITORY, "test-token")

    with pytest.raises(promote.PromotionError, match="HTTPS"):
        api.get_bytes(f"/repos/{REPOSITORY}/actions/artifacts/123/zip")
    assert transport.requests == [(api_url, "Bearer test-token")]


# ── Sequencing and provenance ────────────────────────────────────────────────


def test_failed_or_missing_gate_fails_closed(tmp_path: Path, environment) -> None:
    world = World()
    world.set_source_runs(_run(201, "Source Validation", conclusion="failure"))
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1
    assert not plan.exists()

    world = World()
    world.set_source_runs()
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


def test_foreign_repository_refuses_before_any_api_call(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("GITHUB_REPOSITORY", "attacker/c-two")
    monkeypatch.setenv("GITHUB_REF", "refs/heads/main")
    monkeypatch.setenv("GITHUB_TOKEN", "test-token")

    def explode(*args: Any, **kwargs: Any) -> None:
        raise AssertionError("no API access is permitted from a foreign repository")

    monkeypatch.setattr(promote, "GitHubApi", explode)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


def test_off_main_ref_refuses(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("GITHUB_REPOSITORY", REPOSITORY)
    monkeypatch.setenv("GITHUB_REF", "refs/heads/feature")
    monkeypatch.setenv("GITHUB_TOKEN", "test-token")
    monkeypatch.setattr(promote, "GitHubApi", lambda *args: pytest.fail("no API access off main"))
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


def test_pull_request_candidates_are_rejected(tmp_path: Path, environment) -> None:
    world = _default_world()
    pr_run = _run(101, "Release Candidate", event="pull_request", head_branch="feature")
    world.runs_by_id[101] = pr_run
    world.runs_by_workflow["release-candidate.yml"] = [pr_run]
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1
    assert not plan.exists()


def test_push_candidates_are_rejected(tmp_path: Path, environment) -> None:
    """Push-era candidate runs have no compatibility window: only explicit
    main dispatches may be promoted."""
    world = _default_world()
    push_run = _run(101, "Release Candidate", event="push")
    world.runs_by_id[101] = push_run
    world.runs_by_workflow["release-candidate.yml"] = [push_run]
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1
    assert not plan.exists()


def test_source_sha_mismatch_between_requested_run_and_sha_fails(tmp_path: Path, environment) -> None:
    world = _default_world()
    other = _run(101, "Release Candidate", sha=OTHER_SHA)
    world.runs_by_id[101] = other
    world.runs_by_workflow["release-candidate.yml"] = [other]
    environment(world)
    code, plan, _ = run_prepare(tmp_path, extra=["--source-sha", SOURCE_SHA])
    assert code == 1


def test_candidate_run_id_resolves_source_sha(tmp_path: Path, environment) -> None:
    environment(_default_world())
    code, plan, _ = run_prepare(tmp_path)
    assert code == 0
    assert json.loads(plan.read_text())["source_sha"] == SOURCE_SHA


# ── ZIP safety and API digests ───────────────────────────────────────────────


def _zip_with_members(world: World, artifact_id: int, members: dict[str, bytes]) -> None:
    world.artifact_zips[artifact_id] = _zip_bytes(members)
    for entries in world.artifacts_by_run.values():
        for entry in entries:
            if entry["id"] == artifact_id:
                entry["digest"] = f"sha256:{_sha(world.artifact_zips[artifact_id])}"


@pytest.mark.parametrize("unsafe", ["absolute-path", "parent-escape", "backslash-member"])
def test_unsafe_candidate_zip_names_are_rejected(tmp_path: Path, environment, unsafe: str) -> None:
    world = _default_world()
    members = {
        "absolute-path": {"/etc/passwd": b"x"},
        "parent-escape": {"../escape": b"x"},
        "backslash-member": {"dir\\file": b"x"},
    }[unsafe]
    _zip_with_members(world, 8003, members)
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


def test_duplicate_zip_members_are_rejected(tmp_path: Path, environment) -> None:
    world = _default_world()
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        archive.writestr("rc-manifest.json", b"x")
        archive.writestr("rc-manifest.json", b"y")
    world.artifact_zips[8003] = buffer.getvalue()
    for entries in world.artifacts_by_run.values():
        for entry in entries:
            if entry["id"] == 8003:
                entry["digest"] = f"sha256:{_sha(world.artifact_zips[8003])}"
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


def test_symlink_zip_members_are_rejected(tmp_path: Path, environment) -> None:
    world = _default_world()
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        info = zipfile.ZipInfo("rc-manifest.json")
        info.create_system = 3
        info.external_attr = (stat.S_IFLNK | 0o777) << 16
        archive.writestr(info, "target-file")
    world.artifact_zips[8003] = buffer.getvalue()
    for entries in world.artifacts_by_run.values():
        for entry in entries:
            if entry["id"] == 8003:
                entry["digest"] = f"sha256:{_sha(world.artifact_zips[8003])}"
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


def test_api_digest_mismatch_is_rejected(tmp_path: Path, environment) -> None:
    world = _default_world()
    world.artifact_zips[8002] = world.artifact_zips[8002] + b"tampered"
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


def test_expired_source_validation_artifact_fails_closed(tmp_path: Path, environment) -> None:
    world = _default_world()
    for run_id, entries in world.artifacts_by_run.items():
        if run_id >= 200:
            for entry in entries:
                entry["expired"] = True
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


def test_divergent_duplicate_members_across_artifacts_are_rejected(
    tmp_path: Path, environment
) -> None:
    world = _default_world()
    builder = world.candidate
    blob = _zip_bytes({builder.fastdb_sdist_name: b"fastdb-sdist-DIVERGENT"})
    world.artifact_zips[8004] = blob
    world.artifacts_by_run[101].append({
        "id": 8004, "name": "rc-fastdb-sdist-copy", "size_in_bytes": len(blob),
        "digest": f"sha256:{_sha(blob)}", "expired": False,
    })
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


@pytest.mark.parametrize("target", ["github-release", "pypi"])
def test_candidate_collection_excludes_compiler_cache_diagnostics(
    tmp_path: Path, environment, target: str,
) -> None:
    world = _default_world()
    for artifact_id, platform in ((9001, "aarch64"), (9002, "x86_64")):
        blob = _zip_bytes({"cmake-version.txt": platform.encode()})
        world.artifact_zips[artifact_id] = blob
        world.artifacts_by_run[101].append({
            "id": artifact_id, "name": f"compiler-cache-native-{platform}",
            "size_in_bytes": len(blob), "digest": f"sha256:{_sha(blob)}",
            "expired": False,
        })
    api = environment(world)
    code, plan_path, _ = run_prepare(tmp_path, target=target)
    assert code == 0
    assert json.loads(plan_path.read_text())["source_sha"] == SOURCE_SHA
    assert not (tmp_path / "candidate-download/candidate/cmake-version.txt").exists()
    assert not any(f"/artifacts/{artifact_id}/zip" in path
                   for artifact_id in (9001, 9002) for path in api.paths)


# ── Plan content and wheel-row coverage ─────────────────────────────────────


def test_happy_path_github_release_plan(tmp_path: Path, environment) -> None:
    api = environment(_default_world())
    code, plan_path, body_path = run_prepare(tmp_path)
    assert code == 0
    plan = json.loads(plan_path.read_text())
    assert plan["schema"] == "c-two.release-promotion-plan.v1"
    assert plan["versions"] == {"c_two_version": C_TWO_VERSION, "c3_version": C3_VERSION,
                                "fastdb_version": FASTDB_VERSION,
                                "fastdb_source_rev": FASTDB_SOURCE_REV}
    release = plan["github_release"]
    assert release["tag"] == "c3-v0.2.0"
    assert release["title"] == "c3 0.2.0"
    assert release["tag_exists"] is False and release["release_exists"] is False
    assert release["action"] == "upload"
    expected_assets = [
        name
        for target in CLI_TARGETS
        for name in (_cli_name(target), f"{_cli_name(target)}.sha256")
    ] + ["c3-installer.sh", "c3-installer.ps1",
         "fastdb-LICENSE", "fastdb-THIRD_PARTY_NOTICES.txt", "rc-manifest.json"]
    assert [asset["name"] for asset in release["assets"]] == expected_assets
    for asset in release["assets"]:
        staged = tmp_path / "candidate-download" / asset["staged_path"]
        assert staged.is_file()
        assert _sha(staged.read_bytes()) == asset["sha256"]
    assert plan["source_validation"]["run_id"] == 201
    assert set(plan["source_validation"]["artifacts"]) == set(FULL_LANES)
    # Source proof is checked via metadata only; only candidate ZIPs download.
    assert not any("/actions/artifacts/" in path and int(path.split("/actions/artifacts/")[1].split("/")[0]) >= 9000
                   for path in api.paths)
    body = body_path.read_text()
    assert "actions/runs/101" in body
    assert "Source Validation run:" in body and "actions/runs/201" in body
    assert "Windows 11 ARM64 CI runners" in body
    assert "35 executed Python ABI rows" in body
    assert "debug c3 build" in body
    assert api.paths
    assert all(path.startswith("/repos/") for path in api.paths)


def test_c_two_wheels_still_require_a_single_target() -> None:
    name = f"c_two-{C_TWO_VERSION}-cp312-cp312-macosx_15_0_universal2.whl"
    with pytest.raises(promote.PromotionError, match="exactly one CLI target"):
        promote.map_wheel_rows([name], C_TWO_VERSION)


@pytest.mark.parametrize("target", ["aarch64-apple-darwin", WINDOWS_ARM64_TARGET])
def test_fastdb_provisioning_cannot_substitute_another_targets_proof(
    tmp_path: Path, environment, capsys, target: str,
) -> None:
    builder = CandidateBuilder()
    other = json.loads(builder.members["fastdb-x86_64-apple-darwin.json"])
    builder._rewrite_json(f"fastdb-{target}.json", other)
    world = World(candidate=builder)
    world.set_source_runs(_run(201, "Source Validation"))
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1 and not plan.exists()
    assert "FastDB proof wheel" in capsys.readouterr().err


def test_fastdb_provisioning_missing_wheel_with_null_digest_is_rejected(
    tmp_path: Path, environment, capsys,
) -> None:
    builder = CandidateBuilder()
    receipt = f"fastdb-{WINDOWS_ARM64_TARGET}.json"
    decision = json.loads(builder.members[receipt])
    decision["filename"] = decision["filename"].replace("-cp312", "-1-cp312", 1)
    decision["sha256"] = None
    builder._rewrite_json(receipt, decision)
    world = World(candidate=builder)
    world.set_source_runs(_run(201, "Source Validation"))
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1 and not plan.exists()
    assert "not a distinct retained wheel" in capsys.readouterr().err


@pytest.mark.parametrize("core_version, accepted", [("0.2.4", True), ("0.2.1", False)])
def test_python_fastdb_version_is_independent_of_native_core(
    tmp_path: Path, environment, monkeypatch, core_version, accepted,
) -> None:
    monkeypatch.setitem(globals(), "FASTDB_VERSION", "0.2.2")
    monkeypatch.setitem(globals(), "FASTDB_CORE_VERSION", core_version)
    environment(_default_world())
    code, plan_path, body_path = run_prepare(tmp_path)
    assert code == (0 if accepted else 1)
    if accepted:
        assert json.loads(plan_path.read_text())["versions"]["fastdb_version"] == "0.2.2"
        body = body_path.read_text()
        assert "Native FastDB Core: `0.2.4`" in body
        assert "Optional Python FastDB: `fastdb4py==0.2.2`" in body


def test_free_threaded_wheel_rows_use_real_tag_pairs(tmp_path: Path, environment) -> None:
    """cp314-cp314t is the published free-threaded style; invented
    cp314t-cp314t names are rejected."""
    builder = CandidateBuilder()
    victim = builder.wheel_by_row["x86_64-pc-windows-msvc/3.14t"]
    invented = (f"c_two-{C_TWO_VERSION}-cp314t-cp314t-"
                f"{PLATFORM_TAGS['x86_64-pc-windows-msvc']}.whl")
    data = builder.wheel_data.pop(victim)
    builder.members.pop(victim)
    builder.members.pop(victim + ".sha256")
    builder.wheel_data[invented] = data
    builder._put(invented, data)
    builder.wheels[:] = [e for e in builder.wheels if e["name"] != victim] + [
        e for e in builder.wheels if e["name"] == invented]
    world = World(candidate=builder)
    world.set_source_runs(
        _run(201, "Source Validation"),
    )
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1
    assert not plan.exists()


def test_wheel_rows_must_cover_the_exact_35_row_matrix(tmp_path: Path, environment) -> None:
    builder = CandidateBuilder()
    assert set(CLI_TARGETS) == {
        "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "aarch64-apple-darwin",
        "x86_64-apple-darwin", "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"}
    assert len(builder.wheels) == 35 and len(builder.fastdb_proof) == 9
    # Swap the macOS arm64 3.14 row for a second linux x86_64 3.14 wheel:
    # still 35 unique filenames, but one platform/ABI row is left uncovered.
    victim = builder.wheel_by_row["aarch64-apple-darwin/3.14"]
    builder.drop_member(victim)
    duplicate = (f"c_two-{C_TWO_VERSION}-cp314-cp314-"
                 f"manylinux_2_17_aarch64.manylinux2014_aarch64.whl")
    entry = builder._put(duplicate, b"wheel-bytes-duplicate-row")
    builder.wheels.append(entry)
    world = World(candidate=builder)
    world.set_source_runs(
        _run(201, "Source Validation"),
    )
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


@pytest.mark.parametrize("mutation", [
    "missing-cli", "missing-wheel", "wrong-platform", "python-3.10", "missing-fastdb-proof",
    "wrong-fastdb-platform", "wrong-fastdb-abi", "missing-fastdb-receipt", "renamed-fastdb-receipt",
    "missing-smoke", "missing-standard-user", "missing-wrapper", "missing-abi",
])
def test_windows_arm64_inventory_deviations_fail_closed(tmp_path, environment, capsys, mutation):
    builder = CandidateBuilder()
    target = WINDOWS_ARM64_TARGET
    wheel = builder.wheel_by_row[f"{target}/3.11"]
    missing = {
        "missing-cli": _cli_name(target), "missing-wheel": wheel,
        "missing-fastdb-proof": _fastdb_wheel_name(target, "3.12"),
        "missing-fastdb-receipt": f"fastdb-{target}.json",
        "missing-smoke": f"smoke-{target}.json", "missing-standard-user": f"standard-user-{target}.json",
        "missing-wrapper": f"standard-user-{target}.json.wrapper.json", "missing-abi": f"abi-{target}-3.11.json",
    }
    if mutation in missing:
        builder.drop_member(missing[mutation])
    elif mutation == "renamed-fastdb-receipt":
        name = f"fastdb-{target}.json"
        decision = json.loads(builder.members[name])
        builder.drop_member(name)
        builder._record_receipt("fastdb-unrelated-target.json", decision)
    elif mutation.startswith("wrong-fastdb-"):
        builder.drop_member(_fastdb_wheel_name(target, "3.12"))
        replacement = (_fastdb_wheel_name(WINDOWS_TARGET, "3.12").replace("-cp312", "-1-cp312", 1)
                       if mutation == "wrong-fastdb-platform" else _fastdb_wheel_name(target, "3.11"))
        entry = builder._put(replacement, b"incorrect FastDB proof row")
        builder.fastdb_proof.append(entry)
        # Keep provenance and receipt identities consistent so the row's
        # platform/ABI check, rather than a missing hash binding, rejects it.
        listing = json.loads(builder.members["rc-fastdb-public.json"])
        listing["files"].append({"filename": replacement, "sha256": entry["sha256"],
                                 "packagetype": "bdist_wheel"})
        builder._rewrite_json("rc-fastdb-public.json", listing)
        receipt_name = f"fastdb-{target}.json"
        decision = json.loads(builder.members[receipt_name])
        decision.update(filename=replacement, sha256=entry["sha256"])
        builder._rewrite_json(receipt_name, decision)
    else:
        # Keep 35 unique, hash-bound wheel filenames so the exact matrix,
        # rather than a count or sidecar check, must reject the replacement.
        builder.drop_member(wheel)
        replacement = (wheel.replace("-cp311-cp311-win_arm64", "-1-cp311-cp311-win_amd64")
                       if mutation == "wrong-platform" else wheel.replace("cp311", "cp310"))
        builder.wheels.append(builder._put(replacement, b"incorrect ARM64 row"))
    world = World(candidate=builder)
    world.set_source_runs(_run(201, "Source Validation"))
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1 and not plan.exists()
    error = capsys.readouterr().err
    if mutation == "wrong-platform":
        assert "duplicate wheel row" in error
    elif mutation == "python-3.10":
        assert "outside the expected platform/ABI matrix" in error
    elif mutation.startswith("wrong-fastdb-"):
        assert "FastDB proof wheel" in error


# ── Inventory and manifest deviations ────────────────────────────────────────


@pytest.mark.parametrize(
    "mutation",
    [
        "drop-one-wheel",
        "duplicate-wheel-row",
        "wrong-cli-name",
        "fastdb-license-missing",
        "missing-installers",
        "manifest-digest-lies",
        "receipt-counts-tampered",
        "foreign-member",
        "pr-candidate-context",
        "push-manifest-event",
        "push-context-event",
        "old-context-schema",
        "two-fastdb-sdists",
    ],
)
def test_inventory_and_manifest_deviations_fail_closed(
    tmp_path: Path, environment, mutation: str
) -> None:
    builder = CandidateBuilder()
    manifest = builder.manifest()
    if mutation == "drop-one-wheel":
        builder.drop_member(manifest["wheels"][0]["name"])
    elif mutation == "duplicate-wheel-row":
        removed = manifest["wheels"].pop()
        builder.drop_member(removed["name"])
        manifest["wheels"].append(dict(manifest["wheels"][0]))
    elif mutation == "wrong-cli-name":
        manifest["cli"][0]["name"] = "c3-unexpected-target"
    elif mutation == "fastdb-license-missing":
        manifest["fastdb_proof"] = [
            entry for entry in manifest["fastdb_proof"] if entry["name"] != "fastdb-LICENSE"]
    elif mutation == "missing-installers":
        for name in ("c3-installer.sh", "c3-installer.ps1"):
            builder.drop_member(name)
    elif mutation == "manifest-digest-lies":
        manifest["wheels"][3]["sha256"] = _digest("0")
    elif mutation == "receipt-counts-tampered":
        manifest["counts"]["receipts_by_prefix"]["abi-"] = 34
    elif mutation == "foreign-member":
        builder.members["rogue-file.txt"] = b"unreviewed"
    elif mutation == "pr-candidate-context":
        context = json.loads(builder.members["rc-context.json"])
        context["pull_request_head_sha"] = OTHER_SHA
        builder._rewrite_json("rc-context.json", context)
    elif mutation == "push-manifest-event":
        manifest["event"] = "push"
    elif mutation == "push-context-event":
        context = json.loads(builder.members["rc-context.json"])
        context["event"] = "push"
        builder._rewrite_json("rc-context.json", context)
    elif mutation == "old-context-schema":
        context = json.loads(builder.members["rc-context.json"])
        context["schema"] = "c-two.release-candidate.context.v1"
        builder._rewrite_json("rc-context.json", context)
    elif mutation == "two-fastdb-sdists":
        extra = builder._put("fastdb4py-0.2.1.post.tar.gz", b"second sdist")
        manifest["fastdb_proof"].append(extra)

    world = World(candidate=builder)
    world.set_source_runs(
        _run(201, "Source Validation"),
    )
    if mutation in ("wrong-cli-name", "fastdb-license-missing", "manifest-digest-lies",
                    "receipt-counts-tampered", "duplicate-wheel-row", "drop-one-wheel",
                    "push-manifest-event", "two-fastdb-sdists"):
        world.replace_manifest(manifest)
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1
    assert not plan.exists()


# ── Receipt and Source Validation gate deviations ──────────────────────────────


def _mutated_receipt_world(receipt_name: str, mutate) -> World:
    builder = CandidateBuilder()
    payload = json.loads(builder.members[receipt_name])
    mutate(payload)
    builder._rewrite_json(receipt_name, payload)
    world = World(candidate=builder)
    world.set_source_runs(
        _run(201, "Source Validation"),
    )
    return world


@pytest.mark.parametrize(
    ("receipt_name", "mutate"),
    [
        ("abi-x86_64-unknown-linux-gnu-3.11.json",
         lambda p: p["artifacts"][1].update(sha256=_digest("0"))),
        ("abi-x86_64-unknown-linux-gnu-3.11.json",
         lambda p: p["row"].update(python="3.12")),
        ("abi-x86_64-pc-windows-msvc-3.14t.json",
         lambda p: p["wheel_abi"].update(abi="cp314")),
        ("abi-x86_64-unknown-linux-gnu-3.10.json",
         lambda p: p["fastdb"].update(filename="fastdb4py-0.2.1-cp310-cp310-unheard-of.whl")),
        ("abi-x86_64-unknown-linux-gnu-3.10.json",
         lambda p: p.update(source_commit_sha=OTHER_SHA)),
        ("smoke-x86_64-unknown-linux-gnu.json",
         lambda p: p.update(status="failed")),
        ("smoke-x86_64-unknown-linux-gnu.json",
         lambda p: p["cleanup"].update(processes_exited=False)),
        ("smoke-aarch64-apple-darwin.json",
         lambda p: p.update(borrowed_inputs_invalidated=0)),
        ("smoke-x86_64-apple-darwin.json",
         lambda p: p["checks"][0].update(transport="relay")),
        ("standard-user-x86_64-pc-windows-msvc.json",
         lambda p: p["identity"].update(administrator=True)),
        ("standard-user-x86_64-pc-windows-msvc.json.wrapper.json",
         lambda p: p["cleanup"].update(account_removed=False)),
        ("standard-user-aarch64-pc-windows-msvc.json",
         lambda p: p["identity"].update(administrator=True)),
        ("standard-user-aarch64-pc-windows-msvc.json.wrapper.json",
         lambda p: p["cleanup"].update(account_removed=False)),
        ("abi-aarch64-pc-windows-msvc-3.11.json",
         lambda p: p["row"].update(python="3.10")),
        ("abi-x86_64-pc-windows-msvc-3.10.json", lambda p: p.pop("base_install")),
        ("smoke-aarch64-apple-darwin.json", lambda p: p["base_install"].update(optional_modules_loaded=True)),
        ("smoke-x86_64-unknown-linux-gnu.json", lambda p: p["base_install"].update(optional_dependencies_present=True)),
        ("sdist-proof.json", lambda p: p["lifecycle"]["base_install"].update(transports=["thread"])),
        ("sdist-proof.json", lambda p: p.update(standalone_import="failed")),
        ("sdist-proof.json", lambda p: p["lifecycle"].update(status="failed")),
        ("sdist-proof.json", lambda p: p["lifecycle"]["cleanup"].update(processes_exited=False)),
        ("sdist-proof.json", lambda p: p["lifecycle"]["artifacts"][2].update(sha256=_digest("0"))),
        ("sdist-proof.json", lambda p: p["elf_dynamic"].update(runpath="/tmp/sdk/lib")),
        ("sdist-proof.json", lambda p: p["sdist"].update(sha256=_digest("0"))),
    ],
)
def test_candidate_receipt_deviations_fail_closed(
    tmp_path: Path, environment, receipt_name: str, mutate
) -> None:
    environment(_mutated_receipt_world(receipt_name, mutate))
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1
    assert not plan.exists()


def test_missing_abi_receipt_fails_closed(tmp_path: Path, environment) -> None:
    builder = CandidateBuilder()
    builder.drop_member("abi-x86_64-unknown-linux-gnu-3.11.json")
    world = World(candidate=builder)
    world.set_source_runs(
        _run(201, "Source Validation"),
    )
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


# ── Idempotency and remote conflicts ────────────────────────────────────────


def test_release_idempotency_skips_only_identical_assets(tmp_path: Path, environment) -> None:
    environment(_default_world())
    code, plan, _ = run_prepare(tmp_path)
    assert code == 0
    assets = json.loads(plan.read_text())["github_release"]["assets"]

    published = _default_world()
    published.tag_refs["c3-v0.2.0"] = {
        "ref": "refs/tags/c3-v0.2.0", "object": {"sha": SOURCE_SHA, "type": "commit"}}
    published.releases["c3-v0.2.0"] = {
        "tag_name": "c3-v0.2.0", "name": "c3 0.2.0", "assets": []}
    for index, asset in enumerate(assets):
        published.releases["c3-v0.2.0"]["assets"].append(
            {"id": 5000 + index, "name": asset["name"]})
        published.release_asset_bytes[5000 + index] = (
            tmp_path / "candidate-download" / asset["staged_path"]).read_bytes()
    api = environment(published)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 0
    rerun = json.loads(plan.read_text())
    assert rerun["github_release"]["action"] == "none"
    assert all(a["disposition"] == "verified-existing" for a in rerun["github_release"]["assets"])
    assert any("/releases/assets/" in path for path in api.paths)


def test_release_partial_state_stages_only_missing_uploads(tmp_path: Path, environment) -> None:
    environment(_default_world())
    code, plan, _ = run_prepare(tmp_path)
    assert code == 0
    assets = json.loads(plan.read_text())["github_release"]["assets"]

    partial = _default_world()
    partial.tag_refs["c3-v0.2.0"] = {
        "ref": "refs/tags/c3-v0.2.0", "object": {"sha": SOURCE_SHA, "type": "commit"}}
    partial.releases["c3-v0.2.0"] = {
        "tag_name": "c3-v0.2.0", "name": "c3 0.2.0", "assets": []}
    kept = assets[:5]
    for index, asset in enumerate(kept):
        partial.releases["c3-v0.2.0"]["assets"].append(
            {"id": 5100 + index, "name": asset["name"]})
        partial.release_asset_bytes[5100 + index] = (
            tmp_path / "candidate-download" / asset["staged_path"]).read_bytes()
    environment(partial)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 0
    rerun = json.loads(plan.read_text())
    release = rerun["github_release"]
    assert release["release_exists"] is True
    assert release["action"] == "upload"
    assert {a["name"] for a in release["assets"] if a["disposition"] == "upload"} == \
        {a["name"] for a in assets} - {a["name"] for a in kept}


def test_release_conflicts_fail_closed(tmp_path: Path, environment) -> None:
    environment(_default_world())
    code, plan, _ = run_prepare(tmp_path)
    assert code == 0
    first = json.loads(plan.read_text())["github_release"]["assets"][0]
    first_bytes = (tmp_path / "candidate-download" / first["staged_path"]).read_bytes()

    conflicting = _default_world()
    conflicting.tag_refs["c3-v0.2.0"] = {
        "ref": "refs/tags/c3-v0.2.0", "object": {"sha": SOURCE_SHA, "type": "commit"}}
    conflicting.releases["c3-v0.2.0"] = {
        "tag_name": "c3-v0.2.0", "name": "c3 0.2.0",
        "assets": [{"id": 6001, "name": first["name"]}],
    }
    conflicting.release_asset_bytes[6001] = first_bytes + b"different"
    environment(conflicting)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1

    moved_tag = _default_world()
    moved_tag.tag_refs["c3-v0.2.0"] = {
        "ref": "refs/tags/c3-v0.2.0", "object": {"sha": OTHER_SHA, "type": "commit"}}
    environment(moved_tag)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1

    foreign = _default_world()
    foreign.tag_refs["c3-v0.2.0"] = {
        "ref": "refs/tags/c3-v0.2.0", "object": {"sha": SOURCE_SHA, "type": "commit"}}
    foreign.releases["c3-v0.2.0"] = {
        "tag_name": "c3-v0.2.0", "name": "c3 0.2.0",
        "assets": [{"id": 7001, "name": "rogue-asset.bin"}],
    }
    foreign.release_asset_bytes[7001] = b"rogue"
    environment(foreign)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


def test_pypi_idempotency_is_byte_exact_and_stages_only_missing(
    tmp_path: Path, environment
) -> None:
    environment(_default_world())
    code, plan, _ = run_prepare(tmp_path, target="pypi")
    assert code == 0
    files = json.loads(plan.read_text())["pypi"]["files"]
    assert len(files) == 36
    assert all(entry["name"].startswith("c_two-0.6.0") for entry in files)
    staged_dir = tmp_path / "candidate-download/staging/dist"
    assert sorted(path.name for path in staged_dir.iterdir()) == \
        sorted(entry["name"] for entry in files)

    identical = _default_world()
    identical.pypi = {"urls": [
        {"filename": entry["name"], "digests": {"sha256": entry["sha256"]}} for entry in files]}
    environment(identical)
    code, plan, _ = run_prepare(tmp_path, target="pypi")
    assert code == 0
    rerun = json.loads(plan.read_text())
    assert rerun["pypi"]["action"] == "none"
    assert all(entry["disposition"] == "verified-existing" for entry in rerun["pypi"]["files"])
    # Verified-existing copies are removed from staging; the sources stay.
    assert not any(staged_dir.iterdir())
    extraction = tmp_path / "candidate-download/candidate"
    assert (extraction / files[0]["name"]).is_file()

    partial = _default_world()
    partial.pypi = {"urls": [
        {"filename": files[0]["name"], "digests": {"sha256": files[0]["sha256"]}}]}
    environment(partial)
    code, plan, _ = run_prepare(tmp_path, target="pypi")
    assert code == 0
    partial_plan = json.loads(plan.read_text())
    missing = [entry["name"] for entry in partial_plan["pypi"]["files"]
               if entry["disposition"] == "upload"]
    assert missing == [entry["name"] for entry in files[1:]]
    assert sorted(path.name for path in staged_dir.iterdir()) == sorted(missing)

    conflicting = _default_world()
    conflicting.pypi = {"urls": [
        {"filename": files[0]["name"], "digests": {"sha256": _digest("0")}}]}
    environment(conflicting)
    code, plan, _ = run_prepare(tmp_path, target="pypi")
    assert code == 1

    foreign = _default_world()
    foreign.pypi = {"urls": [
        {"filename": "foreign-0.6.0-py3-none-any.whl", "digests": {"sha256": _digest("1")}}]}
    environment(foreign)
    code, plan, _ = run_prepare(tmp_path, target="pypi")
    assert code == 1


def test_prepare_performs_the_full_read_only_remote_comparison(
    tmp_path: Path, environment
) -> None:
    world = _default_world()
    world.tag_refs["c3-v0.2.0"] = {
        "ref": "refs/tags/c3-v0.2.0", "object": {"sha": SOURCE_SHA, "type": "commit"}}
    world.releases["c3-v0.2.0"] = {
        "tag_name": "c3-v0.2.0", "name": "c3 0.2.0", "assets": []}
    api = environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 0
    payload = json.loads(plan.read_text())
    assert payload["github_release"]["release_exists"] is True
    assert payload["github_release"]["action"] == "upload"
    assert any("/releases/tags/" in path for path in api.paths)
    assert any("/git/ref/tags/" in path for path in api.paths)


# ── Remote collisions with published versions ───────────────────────────────


def test_collision_with_release_published_from_older_source_fails_closed(
    tmp_path: Path, environment
) -> None:
    """A version already released from a different source is a hard collision:
    the tag must never move and the promotion fails closed."""
    environment(_published_release_world())
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1
    assert not plan.exists()


def test_collision_with_pypi_version_published_from_other_bytes_fails_closed(
    tmp_path: Path, environment
) -> None:
    """A published registry version is immutable: differing bytes can never be
    completed from this candidate and the collision fails closed."""
    environment(_published_pypi_world())
    code, plan, _ = run_prepare(tmp_path, target="pypi")
    assert code == 1
    assert not plan.exists()


def test_remote_classifiers_fail_closed_on_published_versions(
    tmp_path: Path, environment
) -> None:
    """The remote-state classifiers themselves never clobber: a tag moved to
    another commit and registry files held under different bytes both refuse."""
    api = environment(_published_release_world())
    assets = [
        {"name": name, "staged_path": f"staging/{name}", "sha256": _digest("9"), "bytes": 1}
        for name in sorted(_canonical_release_asset_names())
    ]
    with pytest.raises(promote.PromotionError, match="refusing to move an existing tag"):
        promote.github_release_state(api, "c3-v0.2.0", "c3 0.2.0", SOURCE_SHA, assets)

    environment(_published_pypi_world())
    files = [
        {"name": _wheel_name(target, python),
         "staged_path": f"staging/dist/{_wheel_name(target, python)}",
         "sha256": _digest("8"), "bytes": 1}
        for target in CLI_TARGETS
        for python in PYTHONS_BY_TARGET[target]
    ]
    files.append({"name": f"c_two-{C_TWO_VERSION}.tar.gz",
                  "staged_path": f"staging/dist/c_two-{C_TWO_VERSION}.tar.gz",
                  "sha256": _digest("8"), "bytes": 1})
    with pytest.raises(promote.PromotionError, match="refusing to clobber"):
        promote.pypi_state("c-two", C_TWO_VERSION, files)


# ── Publish layout roundtrip and fake-gh harness ────────────────────────────


def _workflow_text(name: str) -> str:
    return (_ROOT / ".github" / "workflows" / name).read_text(encoding="utf-8")


def _extract_step(text: str, step_name: str) -> dict[str, str]:
    """Extract one workflow step's `if:` and `run: |` script by name marker."""
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if line.strip() == f"- name: {step_name}":
            condition = ""
            script: list[str] = []
            collecting = False
            for follower in lines[index + 1:]:
                stripped = follower.strip()
                if not collecting:
                    if stripped.startswith("if:"):
                        condition = stripped[3:].strip()
                    elif stripped.startswith("run:"):
                        collecting = True
                    elif stripped.startswith("- name:") or stripped.startswith("uses:"):
                        break
                else:
                    if not follower.strip():
                        script.append("")
                    elif follower.startswith("          "):
                        script.append(follower[10:])
                    else:
                        break
            return {"if": condition, "run": "\n".join(script).strip()}
    raise AssertionError(f"step not found: {step_name}")


_FAKE_GH = """#!/usr/bin/env python3
import os, sys
with open(os.environ["GH_LOG"], "a", encoding="utf-8", newline="\\n") as log:
    log.write("\\0".join(sys.argv[1:]) + "\\n")
sys.exit(0)
"""


def _simulate_staging_roundtrip(tmp_path: Path, destination: Path) -> None:
    """Mirror upload/download-artifact v4 layout semantics: the upload root
    (the staging directory) is stripped from members, so downloading into
    ``destination`` restores the staged tree underneath it."""
    source_root = tmp_path / "candidate-download/staging"
    for file in source_root.rglob("*"):
        if not file.is_file():
            continue
        relative = file.relative_to(source_root)
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(file.read_bytes())


def test_cli_publish_harness_roundtrip(tmp_path: Path, environment) -> None:
    """Drive the workflow's actual publish shell against a fake gh after
    simulating the GitHub artifact roundtrip; a partial existing release
    receives no create and only the missing asset uploads."""
    workflow = _workflow_text("cli-release.yml")
    create_step = _extract_step(workflow, "Create the release when the plan says it is missing")
    upload_step = _extract_step(workflow, "Upload the verified candidate bytes as release assets")
    assert "needs.prepare.outputs.release_exists == 'false'" in create_step["if"]
    assert "--repo" in create_step["run"] and "--repo" in upload_step["run"]
    assert "--clobber" not in upload_step["run"]
    download = re.search(
        r"actions/download-artifact@v4\n\s+with:\n\s+name: promotion-staging\n\s+path: (\S+)",
        workflow)
    assert download is not None and download.group(1) == "staging"

    environment(_default_world())
    code, plan, _ = run_prepare(tmp_path)
    assert code == 0
    original_plan = plan.read_text()
    plan_data = json.loads(original_plan)
    assets = plan_data["github_release"]["assets"]

    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "gh").write_bytes(_FAKE_GH.encode("utf-8"))
    (bin_dir / "gh").chmod(0o755)

    staging_root = tmp_path / "roundtrip-staging"
    _simulate_staging_roundtrip(tmp_path, staging_root)

    def run_publish(*, release_exists: str, tag_exists: str) -> list[list[str]]:
        workspace = tmp_path / f"publish-{release_exists}-{tag_exists}"
        workspace.mkdir()
        (workspace / "promotion-plan.json").write_text(plan.read_text())
        (workspace / "release-body.md").write_text("body")
        for file in staging_root.rglob("*"):
            if file.is_file():
                destination = workspace / "staging" / file.relative_to(staging_root)
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(file.read_bytes())
        log = workspace / "gh.log"
        env = shell_environment(bin_dir)
        env.update({
            "GH_LOG": str(log),
            "GH_TOKEN": "test-token",
            "REPO": REPOSITORY,
            "TAG": plan_data["github_release"]["tag"],
            "TITLE": plan_data["github_release"]["title"],
            "SOURCE_SHA": plan_data["source_sha"],
            "RELEASE_EXISTS": release_exists,
            "TAG_EXISTS": tag_exists,
        })
        for step in (create_step, upload_step):
            if step["if"] and release_exists != "false":
                continue
            result = subprocess.run(
                [posix_shell(), "-c", step["run"]], cwd=workspace, env=env,
                capture_output=True, text=True, encoding="utf-8", timeout=30,
            )
            assert result.returncode == 0, result.stderr
        if not log.exists():
            return []
        return [line.split("\0") for line in log.read_text().splitlines()]

    def uploads(invocations: list[list[str]]) -> set[str]:
        return {arg for args in invocations
                if args[:3] == ["release", "upload", "c3-v0.2.0"]
                for arg in args if arg.startswith("staging/")}

    # Missing release and tag: create targets the candidate source commit.
    invocations = run_publish(release_exists="false", tag_exists="false")
    create_calls = [a for a in invocations if a[:3] == ["release", "create", "c3-v0.2.0"]]
    assert len(create_calls) == 1
    assert "--target" in create_calls[0] and plan_data["source_sha"] in create_calls[0]
    assert "--repo" in create_calls[0] and REPOSITORY in create_calls[0]
    assert uploads(invocations) == {a["staged_path"] for a in assets}

    # Existing tag, missing release: create guards with --verify-tag.
    invocations = run_publish(release_exists="false", tag_exists="true")
    create_calls = [a for a in invocations if a[:3] == ["release", "create", "c3-v0.2.0"]]
    assert "--verify-tag" in create_calls[0]
    assert "--target" not in create_calls[0]

    # Partial existing release: no create at all, and only the missing
    # assets are uploaded — never the verified-existing ones.
    for asset in assets[:5]:
        asset["disposition"] = "verified-existing"
    plan.write_text(json.dumps(plan_data, indent=2, sort_keys=True) + "\n")
    invocations = run_publish(release_exists="true", tag_exists="true")
    assert not any(a[:3] == ["release", "create", "c3-v0.2.0"] for a in invocations)
    expected = {a["staged_path"] for a in assets if a["disposition"] == "upload"}
    assert uploads(invocations) == expected
    assert not uploads(invocations) & {a["staged_path"] for a in assets[:5]}


def test_python_publish_staging_layout_roundtrip(tmp_path: Path, environment) -> None:
    """The pypa publish step consumes staging/dist; after the GitHub artifact
    roundtrip it must hold exactly the registry-missing files."""
    workflow = _workflow_text("python-package-release.yml")
    download = re.search(
        r"actions/download-artifact@v4\n\s+with:\n\s+name: promotion-staging\n\s+path: (\S+)",
        workflow)
    assert download is not None and download.group(1) == "staging"
    packages_dir = re.search(r"packages-dir: (\S+)", workflow)
    assert packages_dir is not None and packages_dir.group(1) == "staging/dist"

    environment(_default_world())
    code, plan, _ = run_prepare(tmp_path, target="pypi")
    assert code == 0
    files = json.loads(plan.read_text())["pypi"]["files"]

    roundtrip = tmp_path / "roundtrip"
    _simulate_staging_roundtrip(tmp_path, roundtrip)
    dist = roundtrip / "dist"
    staged = sorted(path.name for path in dist.iterdir())
    expected = sorted(entry["name"] for entry in files if entry["disposition"] == "upload")
    assert staged == expected
    assert all(name.startswith("c_two-0.6.0") for name in staged)
    # FastDB proof inputs never reach the publish directory.
    assert not any(name.startswith("fastdb4py-") for name in staged)


def test_candidate_rejects_nested_basename_alias(tmp_path, environment):
    world = _default_world()
    _zip_with_members(world, 8003, {"rc-manifest.json": b"root", "nested/rc-manifest.json": b"alias"})
    artifact = next(a for entries in world.artifacts_by_run.values() for a in entries if a["id"] == 8003)
    with pytest.raises(promote.PromotionError, match="non-flat"):
        promote.download_and_extract_artifact(FakeApi(world), artifact, tmp_path / "extract", {})


def test_successful_run_cannot_hide_skipped_manifest_job(tmp_path, environment):
    world = _default_world()
    world.jobs_by_run = {101: [{"name": "context", "status": "completed", "conclusion": "success"},
                               {"name": "manifest", "status": "completed", "conclusion": "skipped"}]}
    environment(world)
    code, _, _ = run_prepare(tmp_path)
    assert code == 1


@pytest.mark.parametrize("workflow_file,workflow_name,run_id,allowed_events", [
    ("release-candidate.yml", "Release Candidate", 101, (promote.CANDIDATE_EVENT,))])
@pytest.mark.parametrize("override", [{"event": "pull_request"}, {"event": "merge_group"},
                                       {"head_branch": "feature"}, {"head_sha": OTHER_SHA},
                                       {"path": ".github/workflows/spoof.yml"}, {"name": "Windows Native"},
                                       {"repository": {"full_name": "attacker/c-two"}},
                                       {"head_repository": {"full_name": "attacker/c-two"}}])
def test_gate_rechecks_provenance_returned_by_the_api(
    override, workflow_file, workflow_name, run_id, allowed_events,
):
    world = _default_world()
    world.runs_by_id[run_id].update(override)
    with pytest.raises(promote.PromotionError):
        promote.validate_gate_run(
            FakeApi(world), world.runs_by_id[run_id], workflow_file, workflow_name,
            SOURCE_SHA, allowed_events)


def test_candidate_gate_rejects_push_events():
    world = _default_world()
    push_candidate = dict(world.runs_by_id[101], event="push")
    with pytest.raises(promote.PromotionError, match="event"):
        promote.validate_gate_run(
            FakeApi(world), push_candidate, "release-candidate.yml", "Release Candidate",
            SOURCE_SHA, (promote.CANDIDATE_EVENT,))


@pytest.mark.parametrize("workflow_name,group,collection,relative", [
    ("cli-release.yml", "github_release", "assets", "staging/c3-test"),
    ("python-package-release.yml", "pypi", "files", "staging/dist/c_two-test.whl"),
])
def test_publish_rechecks_downloaded_staging_bytes(tmp_path, workflow_name, group, collection, relative):
    import subprocess
    step = _extract_step(_workflow_text(workflow_name), "Recheck staged bytes before publication")
    path = tmp_path / relative
    path.parent.mkdir(parents=True)
    path.write_bytes(b"tested bytes")
    entry = {"staged_path": relative, "bytes": path.stat().st_size,
             "sha256": _sha(path.read_bytes()), "disposition": "upload"}
    (tmp_path / "promotion-plan.json").write_text(json.dumps({group: {collection: [entry]}}))
    command = [posix_shell(), "-euo", "pipefail", "-c", step["run"]]
    env = shell_environment(tmp_path / "bin")
    assert subprocess.run(command, cwd=tmp_path, env=env, capture_output=True, timeout=30).returncode == 0
    path.write_bytes(b"forged bytes")
    assert subprocess.run(command, cwd=tmp_path, env=env, capture_output=True, timeout=30).returncode != 0


def test_provisioning_receipt_must_derive_from_the_retained_source(tmp_path, environment, capsys):
    world = _mutated_receipt_world(
        "fastdb-aarch64-unknown-linux-gnu.json",
        lambda payload: payload["built_wheel"].update(derived_from_sdist_sha256=_digest("0")))
    environment(world)
    code, _, _ = run_prepare(tmp_path)
    assert code == 1
    assert "built-wheel derivation" in capsys.readouterr().err


@pytest.mark.parametrize("workflow,target,step_name,group", [
    ("cli-release.yml", "github-release", "Verify the tested candidate and stage release bytes", "github_release"),
    ("python-package-release.yml", "pypi", "Verify the tested candidate and stage distribution files", "pypi"),
])
def test_prepare_summary_exposes_plan_before_environment_approval(
    tmp_path, environment, workflow, target, step_name, group,
):
    environment(_default_world())
    summary = tmp_path / "summary.md"
    summary.write_text("已有摘要\n", encoding="utf-8")
    code, plan_path, _ = run_prepare(tmp_path, target=target, extra=["--summary-output", str(summary)])
    assert code == 0
    plan = json.loads(plan_path.read_text())
    step = _extract_step(_workflow_text(workflow), step_name)
    assert '--summary-output "$GITHUB_STEP_SUMMARY"' in step["run"]
    assert "Publication plan" not in step["run"]
    script = step["run"].split("python - <<'PY'\n", 1)[1].split("\nPY", 1)[0]
    outputs = tmp_path / "outputs"
    result = subprocess.run(
        [sys.executable, "-c", script], cwd=tmp_path,
        env={**os.environ, "GITHUB_STEP_SUMMARY": str(summary), "GITHUB_OUTPUT": str(outputs)},
        capture_output=True, text=True, timeout=30,
    )
    assert result.returncode == 0, result.stderr
    text = summary.read_text(encoding="utf-8")
    assert text.startswith("已有摘要\n")
    assert plan["candidate_run"]["url"] in text and plan["source_sha"] in text
    source = plan["source_validation"]
    for value in (source["run_url"], source["tested_sha"], source["tree_sha"], source["event"]):
        assert value in text
    for job, attempt in source["job_attempts"].items():
        assert f"| {job} | {attempt} |" in text
    for entry in plan[group].get("assets", plan[group].get("files", [])):
        assert entry["name"] in text and entry["sha256"] in text and entry["disposition"] in text
    assert f"action={plan[group]['action']}\n" in outputs.read_text()
