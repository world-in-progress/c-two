"""Tests for .github/scripts/promote_release_candidate.py and the promotion
workflow contracts.

The promotion must be a byte-exact, fail-closed preparation step: unsafe ZIP
members, API digest mismatches, inventory deviations, provenance gaps, and
remote-state conflicts must all refuse promotion, while idempotent reruns
skip only byte-identical remote assets. Fixtures mirror the real candidate
receipt schemas (``c-two.installed-wheel-smoke.v1`` rows with FastDB
decisions, ``cp314``/``cp314t`` tag pairs) and the Windows Native evidence
recorded by tools/ci/windows_native.py. The publish-layout tests simulate the
GitHub artifact roundtrip and drive the workflow's actual publish shell with
a fake ``gh``. Nothing here touches the network or publishes.
"""

from __future__ import annotations

import hashlib
import io
import json
import os
import re
import stat
import subprocess
import urllib.error
import zipfile
from pathlib import Path
from typing import Any

import pytest

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

REPOSITORY = "world-in-progress/c-two"
SOURCE_SHA = "a" * 40
OTHER_SHA = "b" * 40
C_TWO_VERSION = "0.6.0"
C3_VERSION = "0.2.0"
FASTDB_VERSION = "0.2.1"
FASTDB_SOURCE_REV = promote.WINDOWS_NATIVE_FASTDB_SHA
CLI_TARGETS = promote.CLI_TARGETS
WINDOWS_TARGET = promote.WINDOWS_TARGET
PYTHONS = promote.EXPECTED_PYTHONS
PLATFORM_TAGS = {
    "x86_64-unknown-linux-gnu": "manylinux_2_17_x86_64.manylinux2014_x86_64",
    "aarch64-unknown-linux-gnu": "manylinux_2_17_aarch64.manylinux2014_aarch64",
    "aarch64-apple-darwin": "macosx_11_0_arm64",
    "x86_64-apple-darwin": "macosx_11_0_x86_64",
    "x86_64-pc-windows-msvc": "win_amd64",
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
        "event": "push",
        "head_branch": "main",
        "head_sha": sha,
        "status": "completed",
        "conclusion": "success",
        "run_number": run_id,
        "path": ".github/workflows/" + ("release-candidate.yml" if name == "Release Candidate" else "windows-native.yml"),
        "repository": {"full_name": REPOSITORY},
        "head_repository": {"full_name": REPOSITORY},
        "html_url": f"https://github.com/{REPOSITORY}/actions/runs/{run_id}",
    }
    run.update(overrides)
    return run


def _cli_name(target: str) -> str:
    return f"c3-{target}{'.exe' if target == WINDOWS_TARGET else ''}"


def _wheel_name(target: str, python: str) -> str:
    interpreter, abi = promote.PYTHON_ABI_PAIRS[python]
    return f"c_two-{C_TWO_VERSION}-{interpreter}-{abi}-{PLATFORM_TAGS[target]}.whl"


def _fastdb_wheel_name(target: str, python: str) -> str:
    interpreter, abi = promote.PYTHON_ABI_PAIRS[python]
    return f"fastdb4py-{FASTDB_VERSION}-{interpreter}-{abi}-{PLATFORM_TAGS[target]}.whl"


def _matrix_receipt(c3_sha: str | None = None) -> dict[str, Any]:
    """A valid 18-row portable-matrix receipt (mirrors the frozen row contract)."""
    rows = []
    for index, row_id in enumerate(promote.portable_matrix_receipt.EXPECTED_ROW_IDS, start=1):
        payload, client_role, host_role, transport = row_id.split("__")
        rows.append({
            "id": row_id,
            "payload": payload,
            "client_language": client_role.removesuffix("-client"),
            "host_language": host_role.removesuffix("-host"),
            "transport": transport,
            "observed_path": "DirectIpc" if transport == "direct" else "ExplicitRelay",
            "descriptor_sha256": _digest("a"),
            "contract_release_ref": {
                "schema": "c-two.contract-release-ref.v1",
                "contract_schema": "c-two.contract.v2",
                "descriptor_sha256": _digest("a"),
                "crm": {"namespace": f"test.portable-matrix.{payload}",
                        "name": "PortableMatrix", "version": "0.1.0"},
            },
            "fastdb_spec_sha256": None if payload == "no-payload" else _digest("b"),
            "route": {"uid": f"matrix-route-{index:02d}", "revision": 1},
            "logical_result_sha256": _digest("c"),
            "packages": {"client_sha256": _digest("d"), "host_sha256": _digest("e")},
            "c3_sha256": None if transport == "direct" else (c3_sha or _digest("f")),
            "path_counters": {"requests": 1, "responses": 1},
            "status": "passed",
        })
    return {"schema": "c-two.portable-matrix-receipt.v1",
            "evidence_stage": "development", "rows": rows}


def _typescript_receipt(c3_sha: str | None = None) -> dict[str, Any]:
    """A valid 12-row TypeScript real-call receipt (mirrors the frozen contract)."""
    module = promote.typescript_receipt
    observed_paths = {
        "direct-ipc": "DirectIpc",
        "explicit-relay": "ExplicitRelay",
        "relay-aware-local-ipc": "RelayAwareLocalIpc",
        "relay-aware-http": "RelayAwareHttp",
    }
    host_languages = {
        "direct-ipc": "rust",
        "explicit-relay": "python",
        "relay-aware-local-ipc": "python",
        "relay-aware-http": "rust",
    }
    rows = []
    for index, row_id in enumerate(module.EXPECTED_ROW_IDS, start=1):
        payload, mode = row_id.split("__")
        rows.append({
            "id": row_id,
            "payload": payload,
            "mode": mode,
            "host_language": host_languages[mode],
            "observed_path": observed_paths[mode],
            "descriptor_sha256": _digest("a"),
            "contract_release_ref": {
                "schema": "c-two.contract-release-ref.v1",
                "contract_schema": "c-two.contract.v2",
                "descriptor_sha256": _digest("a"),
                "crm": {"namespace": f"test.typescript.{payload}",
                        "name": "Portable", "version": "0.1.0"},
            },
            "fastdb_spec_sha256": None if payload == "no-payload" else _digest("b"),
            "route": {"uid": f"typescript-route-{index}", "revision": 1},
            "logical_result_sha256": _digest("c"),
            "path_counters": {
                "requests": 2 if row_id == module.OPAQUE_ROW_ID else 1,
                "responses": 2 if row_id == module.OPAQUE_ROW_ID else 1,
            },
            "lifetime": {
                "close_idempotent": True,
                "checked_view_invalidated": None if payload == "no-payload" else True,
                "materialized_survived": None if payload == "no-payload" else True,
            },
            "status": "passed",
        })
    return {
        "schema": "c-two.typescript-real-call-receipt.v1",
        "evidence_stage": "development",
        "runtime": {"node_version": "v22.14.0", "platform": "win32",
                    "arch": "AMD64", "browser_runtime": "unverified"},
        "packages": {"fastdb4ts_sha256": _digest("d"),
                     "c2_mem_ffi_sha256": _digest("e"),
                     "c3_sha256": c3_sha or _digest("f")},
        "rows": rows,
        "negative_evidence": {
            "digest_mismatch": {
                "verified": True,
                "row_id": module.DIGEST_ROW_ID,
                "fields": dict(module.FROZEN_FASTDB_DIGEST_MISMATCH_CAUSE),
            },
            "opaque_allocator": {
                "verified": True, "released": 1, "row_id": module.OPAQUE_ROW_ID,
            },
        },
        "cleanup": {
            "host_processes_stopped": True, "relay_processes_stopped": True,
            "node_processes_stopped": True, "response_readers_closed": True,
        },
    }


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
    return {
        "schema": "c-two.installed-wheel-smoke.v1",
        "status": "passed",
        "source_commit_sha": SOURCE_SHA,
        "platform": "TestPlatform-26.1-test",
        "interpreter": "3.12.3 (main, test)",
        "checks": [check, relay],
        "cleanup": {"host_orderly_shutdown": True, "errors": [], "relay_stop": "terminated",
                    "host_stop": "already_exited", "processes_exited": True,
                    "temporary_directory_removed": True},
        "identity": {"administrator": administrator},
        "artifacts": artifacts,
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
            for python in PYTHONS:
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
            for python in PYTHONS:
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
            "schema": "c-two.release-candidate.context.v1",
            "source_commit_sha": SOURCE_SHA,
            "event": "push",
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
        self._record_receipt("fastdb-sdist.json", {"mode": "sdist"})
        for target in CLI_TARGETS:
            self._record_receipt(
                f"fastdb-{target}.json",
                {"mode": "sdist" if target in SDIST_MODE_TARGETS else "wheel",
                 "version": FASTDB_VERSION})

        # 30 executed ABI rows: installed-wheel-smoke receipts bound to rows.
        for target in CLI_TARGETS:
            for python in PYTHONS:
                cli = self.cli_by_name[_cli_name(target)]
                ctwo_name = self.wheel_by_row[f"{target}/{python}"]
                ctwo_data = self.wheel_data[ctwo_name]
                if target in SDIST_MODE_TARGETS:
                    built_name = _fastdb_wheel_name(target, python)
                    if python == "3.12":
                        built = self.retained_fastdb[target]
                    else:
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

        # 5 lifecycle smokes on the cp312 wheel of each target (no decision
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
        windows_cli = self.cli_by_name[_cli_name(WINDOWS_TARGET)]
        windows_ctwo = self.wheel_by_row[f"{WINDOWS_TARGET}/3.12"]
        retained = self.retained_fastdb[WINDOWS_TARGET]
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
        self._record_receipt("standard-user-x86_64-pc-windows-msvc.json", standard)
        self._record_receipt(
            "standard-user-x86_64-pc-windows-msvc.json.wrapper.json", {
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
            "sdk": {"version": FASTDB_VERSION, "source_sha": FASTDB_SOURCE_REV},
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
            "event": "push",
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
        if "/actions/workflows/" in path and "/runs?" in path:
            workflow_file = path.split("/actions/workflows/", 1)[1].split("/runs", 1)[0]
            runs = world.runs_by_workflow.get(workflow_file, [])
            return 200, {"workflow_runs": [dict(run) for run in runs]}
        if "/actions/runs/" in path and "/jobs?" in path:
            run_id = int(path.split("/actions/runs/", 1)[1].split("/jobs", 1)[0])
            run = world.runs_by_id[run_id]
            names = (["context", "manifest"] if run["name"] == "Release Candidate"
                     else [f"{run['runner_year']} / full / MSVC x64"])
            jobs = getattr(world, "jobs_by_run", {}).get(run_id, [
                {"name": name, "status": "completed", "conclusion": "success"} for name in names])
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


def _native_receipt_bytes(payload: dict[str, Any]) -> bytes:
    return json.dumps(payload, indent=2, sort_keys=True).encode() + b"\n"


def _build_native_members(year: str) -> tuple[dict[str, bytes], str]:
    """Build one Windows Native full-scope artifact directory, mirroring
    tools/ci/windows_native.py's run-evidence records."""
    native_c3 = b"native-debug-c3-bytes"
    members: dict[str, bytes] = {}
    matrix = _matrix_receipt(c3_sha=_sha(native_c3))
    typescript = _typescript_receipt(c3_sha=_sha(native_c3))
    fastdb_native = {"name": "fastdb4py-0.2.1-cp312-cp312-win_amd64.whl",
                     "sha256": _digest("1"), "bytes": 1,
                     "distribution": "fastdb4py", "version": FASTDB_VERSION}
    ctwo_native = {"name": "c_two-0.6.0-cp312-cp312-win_amd64.whl",
                   "sha256": _digest("2"), "bytes": 1,
                   "distribution": "c-two", "version": C_TWO_VERSION}
    c3_native = {"name": "c3.exe", "sha256": _sha(native_c3), "bytes": len(native_c3)}
    ordinary = _smoke_body(administrator=True, artifacts=[
        dict(fastdb_native), dict(ctwo_native), dict(c3_native)])
    standard = _smoke_body(administrator=False, artifacts=[
        dict(fastdb_native), dict(ctwo_native), dict(c3_native)])
    wrapper = {
        "schema": "c-two.standard-user-wrapper.v1",
        "status": "passed",
        "errors": [],
        "cleanup": {"processes_exited": True, "workspace_removed": True,
                    "account_removed": True},
        "account_sid": "S-1-5-21-native",
    }
    members[promote.MATRIX_RECEIPT_NAME] = _native_receipt_bytes(matrix)
    members[promote.TYPESCRIPT_RECEIPT_NAME] = _native_receipt_bytes(typescript)
    members[promote.NATIVE_WHEEL_RECEIPT] = _native_receipt_bytes(ordinary)
    members[promote.NATIVE_STANDARD_USER_RECEIPT] = _native_receipt_bytes(standard)
    members[promote.NATIVE_STANDARD_USER_WRAPPER] = _native_receipt_bytes(wrapper)
    members["wheels/c-two/c_two-0.6.0-cp312-cp312-win_amd64.whl"] = b"native wheel"
    members["wheels/fastdb/fastdb4py-0.2.1-cp312-cp312-win_amd64.whl"] = b"native fastdb wheel"
    members[promote.NATIVE_C3_PATH] = native_c3
    members["python-tests.xml"] = b"<testsuite tests='829' failures='0' skipped='0'/>"
    members["typescript-tests.xml"] = b"<testsuite tests='13' failures='0' skipped='0'/>"

    gates = promote.expected_native_full_gates()
    evidence = {
        "schema": "c-two.native-run-evidence.v1",
        "scope": "full",
        "status": "passed",
        "applicable_gates": gates,
        "steps": [{"id": gate, "status": "passed"} for gate in gates],
        "sources": {
            "c-two": {"expected_sha": SOURCE_SHA, "actual_sha": SOURCE_SHA, "verified": True},
            "fastdb": {"expected_sha": promote.WINDOWS_NATIVE_FASTDB_SHA,
                       "actual_sha": promote.WINDOWS_NATIVE_FASTDB_SHA, "verified": True},
        },
        "runner": {"requested_label": year, "os": "Windows Server", "machine": "AMD64"},
        "artifacts": [],
    }
    for name, data in members.items():
        evidence["artifacts"].append({"path": name, "sha256": _sha(data), "bytes": len(data)})
    members["run-evidence.json"] = _native_receipt_bytes(evidence)
    return members, _sha(native_c3)


class World:
    """Everything the promotion script reads: runs, artifacts, remote state."""

    def __init__(self, candidate: CandidateBuilder | None = None) -> None:
        self.candidate = candidate or CandidateBuilder()
        candidate_run = _run(101, "Release Candidate")
        self.runs_by_workflow: dict[str, list[dict[str, Any]]] = {
            "release-candidate.yml": [candidate_run],
            "windows-native.yml": [],
        }
        self.runs_by_id: dict[int, dict[str, Any]] = {101: candidate_run}
        self.artifacts_by_run: dict[int, list[dict[str, Any]]] = {}
        self.artifact_zips: dict[int, bytes] = {}
        self.tag_refs: dict[str, dict[str, Any]] = {}
        self.annotated_tags: dict[str, dict[str, Any]] = {}
        self.releases: dict[str, dict[str, Any]] = {}
        self.release_asset_bytes: dict[int, bytes] = {}
        self.pypi: dict[str, Any] | None = None
        self.native_c3_sha: dict[str, str] = {}
        self._add_candidate_artifacts()

    def set_windows_runs(self, *runs: dict[str, Any]) -> None:
        self.runs_by_workflow["windows-native.yml"] = list(runs)
        for run in runs:
            self.runs_by_id[run["id"]] = run
            if run.get("status", "completed") == "completed":
                self._add_windows_native(run["id"], run["runner_year"])

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

    def _add_windows_native(self, run_id: int, year: str) -> None:
        members, native_c3_sha = _build_native_members(year)
        self.native_c3_sha[year] = native_c3_sha
        self._register_artifact(run_id, 9000 + run_id,
                                f"windows-native-{year}-full-{SOURCE_SHA}", members)

    def mutate_windows_artifacts(self, mutate: Any, *, resync_evidence: bool = True) -> None:
        """Rewrite every windows-native artifact through a member mutation."""
        for run_id, entries in self.artifacts_by_run.items():
            if run_id < 200:
                continue
            for entry in entries:
                with zipfile.ZipFile(io.BytesIO(self.artifact_zips[entry["id"]])) as archive:
                    extracted = {info.filename: archive.read(info.filename)
                                 for info in archive.infolist()}
                mutate(extracted)
                if resync_evidence:
                    evidence = json.loads(extracted["run-evidence.json"])
                    for item in evidence["artifacts"]:
                        item["sha256"] = _sha(extracted[item["path"]])
                        item["bytes"] = len(extracted[item["path"]])
                    extracted["run-evidence.json"] = _native_receipt_bytes(evidence)
                blob = _zip_bytes(extracted)
                self.artifact_zips[entry["id"]] = blob
                entry["digest"] = f"sha256:{_sha(blob)}"


def _default_world() -> World:
    world = World()
    world.set_windows_runs(
        {"id": 201, "runner_year": "windows-2022"} | _run(201, "Windows Native"),
        {"id": 202, "runner_year": "windows-2025"} | _run(202, "Windows Native"),
    )
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
    extra: list[str] | None = None,
    dry_run: bool = False,
) -> tuple[int, Path, Path]:
    download_dir = tmp_path / "candidate-download"
    plan = tmp_path / "promotion-plan.json"
    body = tmp_path / "release-body.md"
    argv = ["prepare", "--target", target,
            "--download-dir", str(download_dir), "--plan-output", str(plan)]
    if target == "github-release":
        argv += ["--release-body-output", str(body)]
    if dry_run:
        argv.append("--dry-run")
    argv += ["--source-sha", SOURCE_SHA]
    argv += extra or []
    return promote.main(argv), plan, body


# ── Sequencing and provenance ────────────────────────────────────────────────


def test_defers_when_the_other_gate_is_still_running(tmp_path: Path, environment) -> None:
    world = World()
    world.set_windows_runs(
        {"runner_year": "windows-2022"}
        | _run(201, "Windows Native", status="in_progress", conclusion=None),
    )
    api = environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == promote.EXIT_DEFERRED
    assert not plan.exists()
    assert any("/actions/workflows/windows-native.yml/runs" in path for path in api.paths)


def test_failed_or_missing_gate_fails_closed(tmp_path: Path, environment) -> None:
    world = World()
    world.set_windows_runs({"runner_year": "windows-2022"}
                           | _run(201, "Windows Native", conclusion="failure"))
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1
    assert not plan.exists()

    world = World()
    world.set_windows_runs()
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
    code, plan, _ = run_prepare(tmp_path, extra=["--candidate-run-id", "101"])
    assert code == 1
    assert not plan.exists()


def test_source_sha_mismatch_between_requested_run_and_sha_fails(tmp_path: Path, environment) -> None:
    world = _default_world()
    other = _run(101, "Release Candidate", sha=OTHER_SHA)
    world.runs_by_id[101] = other
    world.runs_by_workflow["release-candidate.yml"] = [other]
    environment(world)
    code, plan, _ = run_prepare(
        tmp_path, extra=["--candidate-run-id", "101", "--source-sha", SOURCE_SHA])
    assert code == 1


def test_candidate_run_id_resolves_source_sha(tmp_path: Path, environment) -> None:
    environment(_default_world())
    code, plan, _ = run_prepare(tmp_path, extra=["--candidate-run-id", "101"])
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


def test_expired_windows_native_artifact_fails_closed(tmp_path: Path, environment) -> None:
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
    assert set(plan["windows_native"]) == {"windows-2022", "windows-2025"}
    for year, native in plan["windows_native"].items():
        assert native["gates_passed"] == len(promote.expected_native_full_gates())
        # The Native debug c3 is intentionally distinct from the released bytes.
        assert native["native_c3_sha256"] not in {
            asset["sha256"] for asset in release["assets"]}
    body = body_path.read_text()
    assert "actions/runs/101" in body
    assert "Windows 11 desktop is not verified" in body
    assert "debug c3 build" in body
    assert api.paths
    assert all(path.startswith("/repos/") for path in api.paths)


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
    world.set_windows_runs(
        {"id": 201, "runner_year": "windows-2022"} | _run(201, "Windows Native"),
        {"id": 202, "runner_year": "windows-2025"} | _run(202, "Windows Native"),
    )
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1
    assert not plan.exists()


def test_wheel_rows_must_cover_five_platforms_six_abies(tmp_path: Path, environment) -> None:
    builder = CandidateBuilder()
    # Swap the macOS arm64 3.14 row for a second linux x86_64 3.14 wheel:
    # still 30 unique filenames, but one platform/ABI row is left uncovered.
    victim = builder.wheel_by_row["aarch64-apple-darwin/3.14"]
    builder.drop_member(victim)
    duplicate = (f"c_two-{C_TWO_VERSION}-cp314-cp314-"
                 f"manylinux_2_17_aarch64.manylinux2014_aarch64.whl")
    entry = builder._put(duplicate, b"wheel-bytes-duplicate-row")
    builder.wheels.append(entry)
    world = World(candidate=builder)
    world.set_windows_runs(
        {"id": 201, "runner_year": "windows-2022"} | _run(201, "Windows Native"),
        {"id": 202, "runner_year": "windows-2025"} | _run(202, "Windows Native"),
    )
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


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
        manifest["counts"]["receipts_by_prefix"]["abi-"] = 29
    elif mutation == "foreign-member":
        builder.members["rogue-file.txt"] = b"unreviewed"
    elif mutation == "pr-candidate-context":
        context = json.loads(builder.members["rc-context.json"])
        context["pull_request_head_sha"] = OTHER_SHA
        builder._rewrite_json("rc-context.json", context)
    elif mutation == "two-fastdb-sdists":
        extra = builder._put("fastdb4py-0.2.1.post.tar.gz", b"second sdist")
        manifest["fastdb_proof"].append(extra)

    world = World(candidate=builder)
    world.set_windows_runs(
        {"id": 201, "runner_year": "windows-2022"} | _run(201, "Windows Native"),
        {"id": 202, "runner_year": "windows-2025"} | _run(202, "Windows Native"),
    )
    if mutation in ("wrong-cli-name", "fastdb-license-missing", "manifest-digest-lies",
                    "receipt-counts-tampered", "duplicate-wheel-row", "drop-one-wheel",
                    "two-fastdb-sdists"):
        world.replace_manifest(manifest)
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1
    assert not plan.exists()


# ── Receipt and Windows Native gate deviations ──────────────────────────────


def _mutated_receipt_world(receipt_name: str, mutate) -> World:
    builder = CandidateBuilder()
    payload = json.loads(builder.members[receipt_name])
    mutate(payload)
    builder._rewrite_json(receipt_name, payload)
    world = World(candidate=builder)
    world.set_windows_runs(
        {"id": 201, "runner_year": "windows-2022"} | _run(201, "Windows Native"),
        {"id": 202, "runner_year": "windows-2025"} | _run(202, "Windows Native"),
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
    world.set_windows_runs(
        {"id": 201, "runner_year": "windows-2022"} | _run(201, "Windows Native"),
        {"id": 202, "runner_year": "windows-2025"} | _run(202, "Windows Native"),
    )
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


@pytest.mark.parametrize(
    "mutate",
    [
        "matrix-seventeen-rows",
        "typescript-eleven-rows",
        "wrong-evidence-stage",
        "windows-source-mismatch",
        "windows-fastdb-pin-mismatch",
        "windows-evidence-failed",
        "missing-gate-in-steps",
        "gate-not-run",
        "gate-failed",
        "internal-artifact-hash-lie",
        "internal-artifact-missing",
        "matrix-c3-cross-bind-broken",
    ],
)
def test_windows_native_deviations_fail_closed(tmp_path: Path, environment, mutate: str) -> None:
    world = _default_world()

    def mutation(members: dict[str, bytes]) -> None:
        if mutate == "matrix-seventeen-rows":
            matrix = json.loads(members[promote.MATRIX_RECEIPT_NAME])
            matrix["rows"].pop()
            members[promote.MATRIX_RECEIPT_NAME] = _native_receipt_bytes(matrix)
        elif mutate == "typescript-eleven-rows":
            typescript = json.loads(members[promote.TYPESCRIPT_RECEIPT_NAME])
            typescript["rows"].pop()
            members[promote.TYPESCRIPT_RECEIPT_NAME] = _native_receipt_bytes(typescript)
        elif mutate == "wrong-evidence-stage":
            matrix = json.loads(members[promote.MATRIX_RECEIPT_NAME])
            matrix["evidence_stage"] = "candidate"
            members[promote.MATRIX_RECEIPT_NAME] = _native_receipt_bytes(matrix)
        elif mutate == "windows-source-mismatch":
            evidence = json.loads(members["run-evidence.json"])
            evidence["sources"]["c-two"]["actual_sha"] = OTHER_SHA
            members["run-evidence.json"] = _native_receipt_bytes(evidence)
        elif mutate == "windows-fastdb-pin-mismatch":
            evidence = json.loads(members["run-evidence.json"])
            evidence["sources"]["fastdb"]["actual_sha"] = OTHER_SHA
            members["run-evidence.json"] = _native_receipt_bytes(evidence)
        elif mutate == "windows-evidence-failed":
            evidence = json.loads(members["run-evidence.json"])
            evidence["status"] = "failed"
            members["run-evidence.json"] = _native_receipt_bytes(evidence)
        elif mutate == "missing-gate-in-steps":
            evidence = json.loads(members["run-evidence.json"])
            evidence["steps"] = evidence["steps"][:-1]
            members["run-evidence.json"] = _native_receipt_bytes(evidence)
        elif mutate == "gate-not-run":
            evidence = json.loads(members["run-evidence.json"])
            evidence["steps"][0]["status"] = "not_run"
            members["run-evidence.json"] = _native_receipt_bytes(evidence)
        elif mutate == "gate-failed":
            evidence = json.loads(members["run-evidence.json"])
            evidence["steps"][-1]["status"] = "failed"
            members["run-evidence.json"] = _native_receipt_bytes(evidence)
        elif mutate == "internal-artifact-hash-lie":
            evidence = json.loads(members["run-evidence.json"])
            evidence["artifacts"][0]["sha256"] = _digest("0")
            members["run-evidence.json"] = _native_receipt_bytes(evidence)
        elif mutate == "internal-artifact-missing":
            evidence = json.loads(members["run-evidence.json"])
            evidence["artifacts"].append(
                {"path": "wheels/c-two/never-built.whl", "sha256": _digest("1"), "bytes": 1})
            members["run-evidence.json"] = _native_receipt_bytes(evidence)
        elif mutate == "matrix-c3-cross-bind-broken":
            matrix = json.loads(members[promote.MATRIX_RECEIPT_NAME])
            matrix["rows"][1]["c3_sha256"] = _digest("9")
            members[promote.MATRIX_RECEIPT_NAME] = _native_receipt_bytes(matrix)

    if mutate in ("matrix-c3-cross-bind-broken", "internal-artifact-missing",
                  "internal-artifact-hash-lie"):
        # The matrix digest stays internally valid, the phantom artifact record
        # has no bytes to resync, and the digest lie must survive: the
        # run-evidence digest resync is skipped for these cases so the wrong
        # cross-bind, missing file, or lied digest is exactly what is checked.
        world.mutate_windows_artifacts(mutation, resync_evidence=False)
    else:
        world.mutate_windows_artifacts(mutation)
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1
    assert not plan.exists()


def test_missing_2025_native_artifact_fails_closed(tmp_path: Path, environment) -> None:
    world = World()
    world.set_windows_runs({"id": 201, "runner_year": "windows-2022"} | _run(201, "Windows Native"))
    environment(world)
    code, plan, _ = run_prepare(tmp_path)
    assert code == 1


def test_native_fastdb_pin_drift_fails_closed(
    tmp_path: Path, environment, monkeypatch: pytest.MonkeyPatch
) -> None:
    environment(_default_world())
    monkeypatch.setattr(promote, "WINDOWS_NATIVE_FASTDB_SHA", "9" * 40)
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
    assert len(files) == 31
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


def test_dry_run_performs_the_full_read_only_remote_comparison(
    tmp_path: Path, environment
) -> None:
    world = _default_world()
    world.tag_refs["c3-v0.2.0"] = {
        "ref": "refs/tags/c3-v0.2.0", "object": {"sha": SOURCE_SHA, "type": "commit"}}
    world.releases["c3-v0.2.0"] = {
        "tag_name": "c3-v0.2.0", "name": "c3 0.2.0", "assets": []}
    api = environment(world)
    code, plan, _ = run_prepare(tmp_path, dry_run=True)
    assert code == 0
    payload = json.loads(plan.read_text())
    assert payload["dry_run"] is True
    assert payload["github_release"]["release_exists"] is True
    assert payload["github_release"]["action"] == "upload"
    assert any("/releases/tags/" in path for path in api.paths)
    assert any("/git/ref/tags/" in path for path in api.paths)


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
with open(os.environ["GH_LOG"], "a", encoding="utf-8") as log:
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
    (bin_dir / "gh").write_text(_FAKE_GH)
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
        env = dict(os.environ)
        env.update({
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
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
                ["bash", "-c", step["run"]], cwd=workspace, env=env,
                capture_output=True, text=True,
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


@pytest.mark.parametrize("override", [{"event": "pull_request"}, {"head_branch": "feature"},
                                       {"head_sha": OTHER_SHA}, {"path": ".github/workflows/spoof.yml"}])
def test_gate_rechecks_provenance_returned_by_the_api(override):
    world = _default_world()
    world.runs_by_workflow["release-candidate.yml"][0].update(override)
    with pytest.raises(promote.PromotionError):
        promote.resolve_gate(FakeApi(world), "release-candidate.yml", "Release Candidate", SOURCE_SHA)


@pytest.mark.parametrize("workflow_name,group,collection,relative", [
    ("cli-release.yml", "github_release", "assets", "staging/c3-test"),
    ("python-package-release.yml", "pypi", "files", "staging/dist/c_two-test.whl"),
])
def test_publish_rechecks_downloaded_staging_bytes(tmp_path, workflow_name, group, collection, relative):
    import subprocess
    import yaml
    workflow = yaml.safe_load((_ROOT / ".github/workflows" / workflow_name).read_text())
    step = next(step for step in workflow["jobs"]["publish"]["steps"]
                if step.get("name") == "Recheck staged bytes before publication")
    path = tmp_path / relative
    path.parent.mkdir(parents=True)
    path.write_bytes(b"tested bytes")
    entry = {"staged_path": relative, "bytes": path.stat().st_size,
             "sha256": _sha(path.read_bytes()), "disposition": "upload"}
    (tmp_path / "promotion-plan.json").write_text(json.dumps({group: {collection: [entry]}}))
    command = ["bash", "-euo", "pipefail", "-c", step["run"]]
    assert subprocess.run(command, cwd=tmp_path, capture_output=True).returncode == 0
    path.write_bytes(b"forged bytes")
    assert subprocess.run(command, cwd=tmp_path, capture_output=True).returncode != 0
