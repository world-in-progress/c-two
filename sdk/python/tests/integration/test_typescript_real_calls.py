from __future__ import annotations

from contextlib import nullcontext
from copy import deepcopy
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
from typing import Any, Iterator

import pytest

import c_two as cc
from c_two.config.settings import settings
from c_two.transport.registry import _ProcessRegistry
from tests.fixtures.portable_interop import logical_result_sha256
from tests.fixtures.portable_matrix import (
    CRM_CLASSES,
    DESCRIPTOR_PATHS,
    FASTDB_REPOSITORY,
    MatrixArtifacts,
    MatrixRelay,
    RustHost,
    _process_environment,
    _run_checked,
    configure_python_runtime,
    input_lifetime_for,
    resource_for,
    sha256_file,
    wait_for_contract_resolution,
)


REPOSITORY = Path(__file__).resolve().parents[4]
FASTDB_TYPESCRIPT = FASTDB_REPOSITORY / "ts/fastdb4ts"
C2_MEM_TYPESCRIPT = (
    REPOSITORY / "core/foundation/c2-mem-ffi/bindings/typescript"
)
NODE_FIXTURE = (
    REPOSITORY / "sdk/python/tests/fixtures/typescript_real_call.mjs"
)
FASTDB_COMMIT = os.environ.get("C2_TYPESCRIPT_FASTDB_SOURCE_SHA", "ceebed2edbef580ba0a42dcd28dadf9628894523")
sys.path.insert(0, str(REPOSITORY))

from tools.local_rc.typescript_receipt import (  # noqa: E402
    DIGEST_ROW_ID,
    EXPECTED_ROW_IDS,
    OPAQUE_ROW_ID,
    load_and_validate_receipt,
    write_receipt,
)


pytestmark = pytest.mark.timeout(600)
_ROWS: dict[str, dict[str, Any]] = {}
_NEGATIVE_EVIDENCE: dict[str, Any] = {}


def _tree_sha256(root: Path) -> str:
    digest = hashlib.sha256()
    files = sorted(path for path in root.rglob("*") if path.is_file())
    if not files:
        raise AssertionError(f"tree has no files: {root}")
    for path in files:
        relative = path.relative_to(root).as_posix().encode()
        content = bytes.fromhex(sha256_file(path))
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        digest.update(len(content).to_bytes(8, "big"))
        digest.update(content)
    return digest.hexdigest()


def _git(repository: Path, *args: str) -> str:
    return _run_checked(
        ["git", *args],
        cwd=repository,
    ).stdout.strip()


def _pack_npm_package(package: Path, destination: Path) -> Path:
    destination.mkdir(parents=True)
    _run_checked(
        [
            "npm",
            "pack",
            "--json",
            "--pack-destination",
            str(destination),
        ],
        cwd=package,
        timeout=600,
    )
    archives = tuple(destination.glob("*.tgz"))
    if len(archives) != 1:
        raise AssertionError(
            f"npm pack produced {len(archives)} archives in {destination}"
        )
    return archives[0]


@dataclass(frozen=True)
class TypeScriptArtifacts:
    matrix: MatrixArtifacts
    node_root: Path
    node_binary: str
    contract_modules: dict[str, Path]
    payload_modules: dict[str, Path | None]
    generated_tree_sha256: dict[str, str]
    fastdb4ts_sha256: str
    c2_mem_ffi_sha256: str
    node_version: str
    platform_name: str
    architecture: str
    digest_mismatch_cause: dict[str, str]

    @classmethod
    def prepare(cls, work: Path) -> TypeScriptArtifacts:
        configured_fastdb = os.environ.get(
            "C2_TYPESCRIPT_FASTDB_PACKAGE"
        )
        configured_c2_mem = os.environ.get(
            "C2_TYPESCRIPT_C2_MEM_PACKAGE"
        )
        configured_typescript = os.environ.get(
            "C2_TYPESCRIPT_COMPILER_PACKAGE"
        )
        configured_packages = (
            configured_fastdb,
            configured_c2_mem,
            configured_typescript,
        )
        if any(configured_packages) and not all(configured_packages):
            raise AssertionError(
                "candidate TypeScript evidence requires FastDB, C-Two, "
                "and TypeScript tarballs together"
            )
        candidate_packages = all(configured_packages)
        if not candidate_packages:
            assert _git(FASTDB_REPOSITORY, "rev-parse", "HEAD") == FASTDB_COMMIT
            assert _git(FASTDB_REPOSITORY, "status", "--porcelain") == ""
            _run_checked(
                [
                    "cargo",
                    "build",
                    "--manifest-path",
                    str(REPOSITORY / "cli/Cargo.toml"),
                    "--bin",
                    "c3",
                ],
                cwd=REPOSITORY,
                timeout=600,
            )
        matrix_work = work / "matrix"
        matrix_work.mkdir()
        matrix = MatrixArtifacts.prepare(matrix_work)

        if candidate_packages:
            fastdb_archive = Path(configured_fastdb)
            c2_mem_archive = Path(configured_c2_mem)
            typescript_archive = Path(configured_typescript)
            for archive in (
                fastdb_archive,
                c2_mem_archive,
                typescript_archive,
            ):
                if not archive.is_file():
                    raise AssertionError(
                        f"candidate npm tarball is missing: {archive}"
                    )
        else:
            wasm = FASTDB_TYPESCRIPT / "src/wasm/fastdb4ts.wasm"
            if not wasm.is_file():
                if os.name == "nt":
                    emsdk = os.environ.get("EMSDK")
                    if not emsdk:
                        raise AssertionError("EMSDK is required for native Windows WASM builds")
                    emcmake = Path(emsdk) / "upstream/emscripten/emcmake.py"
                    if not emcmake.is_file():
                        raise AssertionError(f"Emscripten CMake helper is missing: {emcmake}")
                    build_directory = work / "fastdb-wasm"
                    _run_checked(
                        [sys.executable, str(emcmake), "cmake",
                         "-S", str(FASTDB_REPOSITORY / "ts/embind"),
                         "-B", str(build_directory), "-G", "Ninja",
                         "-DCMAKE_BUILD_TYPE=Release"],
                        cwd=FASTDB_TYPESCRIPT,
                        timeout=300,
                    )
                    _run_checked(
                        ["cmake", "--build", str(build_directory),
                         "--target", "fastdb4ts", "--config", "Release"],
                        cwd=FASTDB_TYPESCRIPT,
                        timeout=900,
                    )
                else:
                    _run_checked(
                        ["npm", "run", "build:wasm"],
                        cwd=FASTDB_TYPESCRIPT,
                        timeout=900,
                    )
            _run_checked(
                ["npm", "run", "build"],
                cwd=FASTDB_TYPESCRIPT,
                timeout=300,
            )
            packages = work / "packages"
            fastdb_archive = _pack_npm_package(
                FASTDB_TYPESCRIPT,
                packages / "fastdb4ts",
            )
            c2_mem_archive = _pack_npm_package(
                C2_MEM_TYPESCRIPT,
                packages / "c2-mem-ffi",
            )
            (packages / "typescript").mkdir(parents=True)
            typescript_pack = _run_checked(
                [
                    "npm",
                    "pack",
                    "typescript@5.9.3",
                    "--json",
                    "--pack-destination",
                    str(packages / "typescript"),
                ],
                cwd=work,
                timeout=300,
            )
            typescript_entries = json.loads(typescript_pack.stdout)
            typescript_filename = typescript_entries[0]["filename"]
            typescript_archive = (
                packages / "typescript" / typescript_filename
            )

        node_root = work / "node"
        node_root.mkdir()
        (node_root / "package.json").write_text(
            json.dumps(
                {
                    "name": "c-two-typescript-real-call-development",
                    "private": True,
                    "type": "module",
                },
                sort_keys=True,
            ),
            encoding="utf-8",
        )
        _run_checked(
            [
                "npm",
                "install",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                str(fastdb_archive),
                str(c2_mem_archive),
                str(typescript_archive),
            ],
            cwd=node_root,
            timeout=300,
        )

        generated_root = node_root / "generated"
        generated_root.mkdir()
        contract_modules: dict[str, Path] = {}
        payload_modules: dict[str, Path | None] = {}
        generated_tree_sha256: dict[str, str] = {}
        for payload, descriptor in DESCRIPTOR_PATHS.items():
            destination = generated_root / payload
            _run_checked(
                [
                    str(matrix.c3_binary),
                    "contract",
                    "codegen",
                    "typescript",
                    str(descriptor),
                    "--out-dir",
                    str(destination),
                ],
                cwd=REPOSITORY,
            )
            canonical = (destination / "metadata/contract.json").read_bytes()
            assert (
                hashlib.sha256(canonical).hexdigest()
                == matrix.contracts[payload].descriptor_sha256
            )
            generated_tree_sha256[payload] = _tree_sha256(destination)

        config = {
            "compilerOptions": {
                "target": "ES2022",
                "module": "NodeNext",
                "moduleResolution": "NodeNext",
                "lib": ["ES2022", "DOM"],
                "strict": True,
                "skipLibCheck": False,
                "rootDir": str(generated_root),
                "outDir": str(node_root / "dist"),
            },
            "include": [str(generated_root / "**/*.ts")],
        }
        config_path = node_root / "tsconfig.json"
        config_path.write_text(
            json.dumps(config, indent=2, sort_keys=True),
            encoding="utf-8",
        )
        tsc = node_root / "node_modules/typescript/bin/tsc"
        node_binary = shutil.which("node")
        if node_binary is None or not tsc.is_file():
            raise AssertionError("Node and the pinned TypeScript compiler are required")
        _run_checked(
            [node_binary, str(tsc), "--project", str(config_path)],
            cwd=node_root,
            timeout=300,
        )

        for payload in DESCRIPTOR_PATHS:
            output = node_root / "dist" / payload / "typescript"
            contract_modules[payload] = output / "c_two_contract.js"
            candidates = tuple(
                output.glob("payloads/*/fastdb_payload_*.js")
            )
            if payload == "no-payload":
                assert candidates == ()
                payload_modules[payload] = None
            else:
                if len(candidates) != 1:
                    raise AssertionError(
                        f"{payload} generated {len(candidates)} payload modules"
                    )
                payload_modules[payload] = candidates[0]
            assert contract_modules[payload].is_file()

        shutil.copyfile(NODE_FIXTURE, node_root / NODE_FIXTURE.name)
        node_version = _run_checked(
            [node_binary, "--version"],
            cwd=node_root,
        ).stdout.strip()
        cause = json.loads(
            (
                REPOSITORY
                / "tests/fixtures/fastdb-digest-mismatch-cause.json"
            ).read_text(encoding="utf-8")
        )
        return cls(
            matrix=matrix,
            node_root=node_root,
            node_binary=node_binary,
            contract_modules=contract_modules,
            payload_modules=payload_modules,
            generated_tree_sha256=generated_tree_sha256,
            fastdb4ts_sha256=sha256_file(fastdb_archive),
            c2_mem_ffi_sha256=sha256_file(c2_mem_archive),
            node_version=node_version,
            platform_name=sys.platform,
            architecture=platform.machine(),
            digest_mismatch_cause=cause,
        )

    def run_node(
        self,
        *,
        row_id: str,
        payload: str,
        mode: str,
        endpoint: str,
        route_name: str,
    ) -> dict[str, Any]:
        config = {
            "contractModule": str(self.contract_modules[payload]),
            "payloadModule": (
                None
                if self.payload_modules[payload] is None
                else str(self.payload_modules[payload])
            ),
            "payload": payload,
            "mode": mode,
            "endpoint": endpoint,
            "routeName": route_name,
            "proveDigestCause": row_id == DIGEST_ROW_ID,
            "proveOpaqueAllocator": row_id == OPAQUE_ROW_ID,
            "fastdbDigestMismatchCause": self.digest_mismatch_cause,
        }
        config_path = self.node_root / f"{row_id}.json"
        config_path.write_text(
            json.dumps(config, sort_keys=True),
            encoding="utf-8",
        )
        completed = _run_checked(
            [
                self.node_binary,
                str(self.node_root / NODE_FIXTURE.name),
                str(config_path),
            ],
            cwd=self.node_root,
            environment=_process_environment(),
            timeout=120,
        )
        receipts = [
            json.loads(line.removeprefix("NODE_RECEIPT "))
            for line in completed.stdout.splitlines()
            if line.startswith("NODE_RECEIPT ")
        ]
        if len(receipts) != 1:
            raise AssertionError(
                f"Node emitted {len(receipts)} receipts\n"
                f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}"
            )
        return receipts[0]


@pytest.fixture(scope="module")
def typescript_artifacts() -> Iterator[TypeScriptArtifacts]:
    with tempfile.TemporaryDirectory(
        prefix="c-two-typescript-real-calls-",
    ) as directory:
        yield TypeScriptArtifacts.prepare(Path(directory))


@pytest.fixture(autouse=True)
def _clean_python_runtime() -> Iterator[None]:
    previous_relay = settings._relay_anchor_address  # noqa: SLF001
    previous_threshold = settings._shm_threshold  # noqa: SLF001
    _ProcessRegistry.reset()
    settings.relay_anchor_address = None
    settings.shm_threshold = None
    yield
    _ProcessRegistry.reset()
    settings._relay_anchor_address = previous_relay  # noqa: SLF001
    settings._shm_threshold = previous_threshold  # noqa: SLF001


def _route_name(row_id: str) -> str:
    digest = hashlib.sha256(row_id.encode()).hexdigest()[:16]
    return f"typescript-{os.getpid()}-{digest}"


def _ipc_address(row_id: str) -> str:
    digest = hashlib.sha256(row_id.encode()).hexdigest()[:16]
    return f"ipc://c2_typescript_{os.getpid()}_{digest}"


def _host_language(mode: str) -> str:
    return "rust" if mode in ("direct-ipc", "relay-aware-http") else "python"


def _run_python_host(
    artifacts: TypeScriptArtifacts,
    *,
    row_id: str,
    payload: str,
    mode: str,
    route_name: str,
    relay_url: str | None,
) -> tuple[dict[str, Any], int]:
    resource = resource_for(payload)
    configure_python_runtime(
        server=True,
        client=False,
        relay_url=relay_url,
    )
    cc.register(
        CRM_CLASSES[payload],
        resource,
        name=route_name,
        input_lifetime=input_lifetime_for(payload),
    )
    address = cc.server_address()
    assert address is not None
    try:
        if relay_url is not None:
            wait_for_contract_resolution(
                relay_url,
                route_name,
                artifacts.matrix.generated_python[payload],
            )
        receipt = artifacts.run_node(
            row_id=row_id,
            payload=payload,
            mode=mode,
            endpoint=address if mode == "direct-ipc" else relay_url or "",
            route_name=route_name,
        )
        resource.assert_lifetime()
        return receipt, resource.calls
    finally:
        _ProcessRegistry.reset()
        resource.close()


def _run_rust_host(
    artifacts: TypeScriptArtifacts,
    *,
    row_id: str,
    payload: str,
    mode: str,
    route_name: str,
    address: str,
    relay_url: str | None,
) -> tuple[dict[str, Any], int]:
    host = RustHost(
        artifacts.matrix,
        payload=payload,
        address=address,
        route_name=route_name,
        relay_url=relay_url,
    )
    try:
        if relay_url is not None:
            wait_for_contract_resolution(
                relay_url,
                route_name,
                artifacts.matrix.generated_python[payload],
            )
        receipt = artifacts.run_node(
            row_id=row_id,
            payload=payload,
            mode=mode,
            endpoint=address if mode == "direct-ipc" else relay_url or "",
            route_name=route_name,
        )
    finally:
        calls = host.stop()
    return receipt, calls


@pytest.mark.parametrize("row_id", EXPECTED_ROW_IDS, ids=EXPECTED_ROW_IDS)
def test_generated_typescript_calls_real_hosts(
    row_id: str,
    typescript_artifacts: TypeScriptArtifacts,
) -> None:
    payload, mode = row_id.split("__")
    route_name = _route_name(row_id)
    address = _ipc_address(row_id)
    host_language = _host_language(mode)
    relay_context = (
        nullcontext(None)
        if mode == "direct-ipc"
        else MatrixRelay(typescript_artifacts.matrix, row_id)
    )
    with relay_context as relay:
        relay_url = relay.url if relay is not None else None
        if host_language == "python":
            receipt, calls = _run_python_host(
                typescript_artifacts,
                row_id=row_id,
                payload=payload,
                mode=mode,
                route_name=route_name,
                relay_url=relay_url,
            )
        else:
            receipt, calls = _run_rust_host(
                typescript_artifacts,
                row_id=row_id,
                payload=payload,
                mode=mode,
                route_name=route_name,
                address=address,
                relay_url=relay_url,
            )

    expected_calls = 2 if row_id == OPAQUE_ROW_ID else 1
    assert calls == expected_calls
    assert receipt["logicalResultSha256"] == logical_result_sha256(payload)
    assert receipt["cleanup"] == {
        "responseShmReaderClosed": True,
        "transportClosedIdempotently": True,
    }
    observations = receipt["observations"]
    assert len(observations) == expected_calls
    route_uids = {observation["routeUid"] for observation in observations}
    route_revisions = {
        observation["routeRevision"] for observation in observations
    }
    observed_paths = {observation["path"] for observation in observations}
    assert len(route_uids) == 1
    assert len(route_revisions) == 1
    assert len(observed_paths) == 1
    assert next(iter(route_revisions)) > 0

    if row_id == DIGEST_ROW_ID:
        _NEGATIVE_EVIDENCE["digest_mismatch"] = {
            "verified": True,
            "row_id": row_id,
            "fields": deepcopy(typescript_artifacts.digest_mismatch_cause),
        }
    if row_id == OPAQUE_ROW_ID:
        assert receipt["opaqueAllocator"] == {"rejected": True, "released": 1}
        _NEGATIVE_EVIDENCE["opaque_allocator"] = {
            "verified": True,
            "released": 1,
            "row_id": row_id,
        }

    facts = typescript_artifacts.matrix.contracts[payload]
    _ROWS[row_id] = {
        "id": row_id,
        "payload": payload,
        "mode": mode,
        "host_language": host_language,
        "observed_path": next(iter(observed_paths)),
        "descriptor_sha256": facts.descriptor_sha256,
        "contract_release_ref": deepcopy(facts.release_ref),
        "fastdb_spec_sha256": facts.fastdb_spec_sha256,
        "route": {
            "uid": next(iter(route_uids)),
            "revision": next(iter(route_revisions)),
        },
        "logical_result_sha256": receipt["logicalResultSha256"],
        "path_counters": {
            "requests": len(observations),
            "responses": len(observations),
        },
        "lifetime": {
            "close_idempotent": receipt["lifecycle"]["closeIdempotent"],
            "checked_view_invalidated": receipt["lifecycle"][
                "checkedViewInvalidated"
            ],
            "materialized_survived": receipt["lifecycle"][
                "materializedSurvived"
            ],
        },
        "status": "passed",
    }


def test_typescript_real_calls_write_strict_development_receipt(
    typescript_artifacts: TypeScriptArtifacts,
) -> None:
    assert tuple(_ROWS) == EXPECTED_ROW_IDS
    assert set(_NEGATIVE_EVIDENCE) == {
        "digest_mismatch",
        "opaque_allocator",
    }
    evidence_stage = os.environ.get(
        "C2_TYPESCRIPT_EVIDENCE_STAGE",
        "development",
    )
    configured = os.environ.get("C2_TYPESCRIPT_RECEIPT")
    receipt_path = (
        Path(configured)
        if configured
        else REPOSITORY
        / "target/local-rc/typescript-real-call-receipt.v1.json"
    )
    receipt = {
        "schema": "c-two.typescript-real-call-receipt.v1",
        "evidence_stage": evidence_stage,
        "runtime": {
            "node_version": typescript_artifacts.node_version,
            "platform": typescript_artifacts.platform_name,
            "arch": typescript_artifacts.architecture,
            "browser_runtime": "unverified",
        },
        "packages": {
            "fastdb4ts_sha256": typescript_artifacts.fastdb4ts_sha256,
            "c2_mem_ffi_sha256": typescript_artifacts.c2_mem_ffi_sha256,
            "c3_sha256": typescript_artifacts.matrix.c3_sha256,
        },
        "rows": [_ROWS[row_id] for row_id in EXPECTED_ROW_IDS],
        "negative_evidence": deepcopy(_NEGATIVE_EVIDENCE),
        "cleanup": {
            "host_processes_stopped": True,
            "relay_processes_stopped": True,
            "node_processes_stopped": True,
            "response_readers_closed": True,
        },
    }
    write_receipt(
        receipt_path,
        receipt,
        expected_stage=evidence_stage,
    )
    assert load_and_validate_receipt(
        receipt_path,
        expected_stage=evidence_stage,
    ) == receipt
