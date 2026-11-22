from __future__ import annotations

from contextlib import ExitStack, nullcontext
from copy import deepcopy
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from textwrap import dedent
from typing import Any, Iterator

import pytest

import c_two as cc
from c_two.crm.descriptor import export_descriptor
from c_two.transport.registry import _reset_default_runtime
from fastdb4py.payload import Payload
from tests.fixtures.ipc_version_peer import ipc_version_peer_binary, running_version_peer
from tests.fixtures.process_control import readline_with_timeout
from tests.fixtures.typescript_project import write_node_tsconfig
from tests.fixtures.portable_interop import (
    FASTDB_REPOSITORY, RECORD_SPEC, build_record_payload, logical_result_sha256,
)
from tests.fixtures.portable_matrix import (
    CRM_CLASSES,
    DESCRIPTOR_PATHS,
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
FASTDB_COMMIT = os.environ.get("C2_TYPESCRIPT_FASTDB_SOURCE_SHA", "ff097cca9c29cadc289e2a437f4720ecc56015c4")
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


def _fastdb_npm_build_command() -> list[str]:
    command = ["npm"]
    if os.name == "nt":
        configured = os.environ.get("C2_FASTDB_NPM_SCRIPT_SHELL")
        shell = (
            Path(configured)
            if configured
            else Path(os.environ.get("ProgramFiles", r"C:\Program Files"))
            / "Git/bin/bash.exe"
        )
        if not shell.is_file():
            raise AssertionError(
                "FastDB npm build requires Git Bash; configure "
                f"C2_FASTDB_NPM_SCRIPT_SHELL (not found: {shell})"
            )
        command.extend(["--script-shell", str(shell)])
    return [*command, "run", "build"]


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
            from tests.fixtures.typescript_project import require_c2_mem_node_build
            require_c2_mem_node_build(REPOSITORY)
            assert _git(FASTDB_REPOSITORY, "rev-parse", "HEAD") == FASTDB_COMMIT
            assert _git(FASTDB_REPOSITORY, "status", "--porcelain") == ""
            # MatrixArtifacts.prepare resolves the CLI through the shared helper,
            # which builds it only when no binary exists. Rebuilding an existing
            # c3 here would replace the relinked artifact that the CI gate has
            # already retained and hashed, so the downloadable c3.exe could no
            # longer match the binary these receipts name.
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
                _fastdb_npm_build_command(),
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

        config_path = write_node_tsconfig(node_root)
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
        node_runtime = json.loads(_run_checked(
            [node_binary, "-p", "JSON.stringify({version:process.version,platform:process.platform,arch:process.arch})"],
            cwd=node_root,
        ).stdout)
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
            node_version=node_runtime["version"],
            platform_name=node_runtime["platform"],
            architecture=node_runtime["arch"],
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
    _reset_default_runtime()
    yield
    _reset_default_runtime()


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
        _reset_default_runtime()
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
        "nodeConnectorCreated": mode in {"direct-ipc", "relay-aware-local-ipc"},
        "nodeConnectorClosed": mode in {"direct-ipc", "relay-aware-local-ipc"},
        "transportClosedIdempotently": True,
    }
    observations = receipt["observations"]
    assert len(observations) == expected_calls
    assert {observation["routeName"] for observation in observations} == {route_name}
    assert all("routeUid" not in observation for observation in observations)
    observed_paths = {observation["path"] for observation in observations}
    assert len(observed_paths) == 1

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
            "name": route_name,
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


def test_typescript_real_connector_version_mismatch(
    typescript_artifacts: TypeScriptArtifacts, ipc_version_peer_binary: Path,
) -> None:
    script = typescript_artifacts.node_root / "ipc-version.mjs"
    script.write_text(dedent("""\
        import assert from 'node:assert/strict';
        import { pathToFileURL } from 'node:url';
        import { createNodeIpcConnector } from '@c-two/c2-mem-ffi';
        const [modulePath, address, root] = process.argv.slice(2);
        const contract = await import(pathToFileURL(modulePath).href);
        const connector = createNodeIpcConnector(root ? {ipcRoot: root} : {});
        let connects = 0;
        const transport = contract.createIpcEncodedTransport(address, {
          connect: async (target) => { connects++; return connector(target); },
        });
        try {
          await assert.rejects(transport.prepare('version', contract.CONTRACT), (error) => {
            assert.ok(error instanceof contract.C2IpcTransportError);
            assert.equal(error.category, 'protocol_version_mismatch');
            assert.equal(error.localProtocolVersion, 17);
            assert.equal(error.peerProtocolVersion, 16);
            return true;
          });
          assert.equal(connects, 1);
        } finally { await transport.close(); await connector.close(); }
        """), encoding="utf-8")
    with running_version_peer(ipc_version_peer_binary, "mismatch16") as (address, context):
        _run_checked(
            [typescript_artifacts.node_binary, str(script),
             str(typescript_artifacts.contract_modules["no-payload"]), address, context.root or ""],
            cwd=typescript_artifacts.node_root, timeout=10,
        )


def test_generated_typescript_subscribes_via_ipc_and_two_relay_anchors(
    typescript_artifacts: TypeScriptArtifacts, monkeypatch: pytest.MonkeyPatch,
) -> None:
    @cc.crm(namespace="test.portable-matrix.typescript", version="0.1.0")
    class Telemetry:
        @cc.transfer(output=RECORD_SPEC)
        @cc.topic
        def state(self) -> Payload: ...

    artifacts = typescript_artifacts
    root = artifacts.node_root
    contract = root / "topic.contract.json"
    contract.write_text(export_descriptor(Telemetry), encoding="utf-8")
    _run_checked(
        [str(artifacts.matrix.c3_binary), "contract", "codegen", "typescript",
         str(contract), "--out-dir", str(root / "generated" / "topic")],
        cwd=root, environment=_process_environment(),
    )
    _run_checked(
        [artifacts.node_binary, str(root / "node_modules/typescript/bin/tsc"),
         "--project", str(write_node_tsconfig(root))],
        cwd=root, timeout=300,
    )

    script = root / "subscribe.mjs"
    script.write_text(dedent("""\
        import assert from 'node:assert/strict';
        import {createInterface} from 'node:readline';
        import {initPayload} from 'fastdb4ts/payload';
        import {createNodeIpcConnector} from '@c-two/c2-mem-ffi';
        import * as generated from './dist/topic/typescript/c_two_contract.js';

        const [address, first, second, localRoot, expected, firstRoute, secondRoute] = process.argv.slice(2);
        await initPayload();
        const connector = createNodeIpcConnector({
          ...(localRoot ? {ipcRoot: localRoot} : {}), maxOutstandingCalls: 3,
        });
        const prepared = [];
        const connect = async (...args) => {
          const endpoint = await connector(...args);
          return new Proxy(endpoint, {
            get(target, name) {
              if (name === 'prepare') return async (...values) => {
                const result = await target.prepare(...values);
                prepared.push(result);
                return result;
              };
              const value = Reflect.get(target, name);
              return typeof value === 'function' ? value.bind(target) : value;
            },
          });
        };
        const fetch = () => { throw new Error('unexpected JavaScript HTTP request'); };
        const direct = generated.createIpcEncodedTransport(address, {connect});
        const local = generated.createRelayAwareHttpEncodedTransport(first, {connect, fetch});
        const fallback = generated.createRelayAwareHttpEncodedTransport(second, {connect, fetch});
        const explicit = generated.createHttpRelayEncodedTransport(first, {connect, fetch});
        const directClient = new generated.ContractClient(direct, firstRoute);
        const localClient = new generated.ContractClient(local, firstRoute);
        const fallbackClient = new generated.ContractClient(fallback, secondRoute);
        const explicitClient = new generated.ContractClient(explicit, firstRoute);
        const controls = createInterface({input: process.stdin});
        const replies = controls[Symbol.asyncIterator]();
        const host = async command => {
          console.log(command);
          assert.equal((await replies.next()).value, 'ok');
        };
        const opened = [];
        const track = promise => promise.then(sub => { opened.push(sub); return sub; });
        const options = {timeoutMs: 10_000};
        const verify = (delivery, payload = delivery.value) => {
          assert.equal(delivery.seq, 1n);
          assert.equal(delivery.dropped, 0n);
          assert.ok(Object.isFrozen(delivery));
          assert.equal(Buffer.from(payload.binaryBytes()).toString('hex'), expected);
        };
        try {
          const [localSub, fallbackSub] = await Promise.all([
            track(localClient.hold_subscribe_topic_0_state(options)),
            track(fallbackClient.subscribe_topic_0_state(options)),
          ]);
          const paths = prepared.map(result => result.path);
          assert.ok(paths.includes('RelayAwareLocalIpc'));
          assert.ok(paths.includes(localRoot ? 'RelayAwareHttp' : 'RelayAwareLocalIpc'));
          assert.equal(connector.stats().call_execution.used_operations, '2');
          await assert.rejects(explicitClient.subscribe_topic_0_state(options), error => error.code === 717);
          assert.equal(localSub.startSeq, 1n);
          assert.equal(fallbackSub.startSeq, 1n);
          const held = (await localSub.next()).value;
          verify(held, held.value.value);
          const copied = (await fallbackSub.next()).value;
          try { verify(copied); } finally { copied.value.dispose(); }
          for (const sub of [localSub, fallbackSub]) {
            const receipt = await sub.close();
            assert.equal(receipt.completed, true);
            assert.equal(receipt.error.code, 718);
            assert.equal(receipt.completeItems, 1n);
          }
          assert.equal(Buffer.from(held.value.value.binaryBytes()).toString('hex'), expected);
          const alias = held.value.buffer.subarray();
          held.value.release();
          assert.equal(alias.byteLength, 0);

          const [ipcSub, httpSub] = await Promise.all([
            track(directClient.subscribe_topic_0_state(options)),
            track(explicitClient.subscribe_topic_0_state({...options, start: 'next'})),
          ]);
          assert.equal(httpSub.startSeq, 2n);
          assert.ok(prepared.some(result => result.path === 'ExplicitRelay'));
          const frame = (await ipcSub.next()).value;
          try { verify(frame); } finally { frame.value.dispose(); }
          assert.equal((await httpSub.close()).completeItems, 0n);
          assert.equal((await ipcSub.close()).completed, true);
          assert.equal(connector.stats().call_execution.used_operations, '0');
          await assert.rejects(explicit.subscribe(firstRoute, generated.CONTRACT, 'missing', data => data, options),
            error => error.code === 713);

          const next = {...options, start: 'next'};
          const previous = await Promise.all([
            track(localClient.subscribe_topic_0_state(next)),
            track(explicitClient.subscribe_topic_0_state(next)),
          ]);
          await host('unregister');
          await Promise.all(previous.map(sub => assert.rejects(sub.next(), error =>
            error.code === 718 && error.details.cause === 'route_unregistered')));
          await host('register');
          const replacement = await track(explicitClient.subscribe_topic_0_state(options));
          assert.equal(replacement.startSeq, 1n);
          const replacementFrame = (await replacement.next()).value;
          try { verify(replacementFrame); } finally { replacementFrame.value.dispose(); }
          assert.equal((await replacement.close()).completed, true);
        } finally {
          await Promise.allSettled(opened.map(sub => sub.close(2_000)));
          await Promise.allSettled([direct, local, fallback, explicit].map(transport => transport.close()));
          connector.close();
          controls.close();
          process.stdin.pause();
        }
        """), encoding="utf-8")

    first_route = _route_name("topic-first")
    second_route = _route_name("topic-second")
    value = build_record_payload()
    try:
        with ExitStack() as stack:
            local_root = ""
            remote_root = ""
            if os.name != "nt":
                local_root = stack.enter_context(tempfile.TemporaryDirectory(prefix="c2ts-", dir="/tmp"))
                remote_root = stack.enter_context(tempfile.TemporaryDirectory(prefix="c2ts-", dir="/tmp"))

            def start_host(name: str, ipc_root: str) -> tuple[cc.Runtime, MatrixRelay]:
                with monkeypatch.context() as process_env:
                    if ipc_root:
                        process_env.setenv("C2_IPC_ROOT", ipc_root)
                    relay = stack.enter_context(MatrixRelay(artifacts.matrix, name))
                runtime = cc.Runtime()
                stack.callback(runtime.shutdown)
                if ipc_root:
                    runtime.set_local_endpoint(root=ipc_root)
                runtime.set_relay_anchor(relay.url)
                runtime.register(Telemetry, object(), name=name, initial_topics={"state": value})
                return runtime, relay

            host, first = start_host(first_route, local_root)
            _, second = start_host(second_route, remote_root)
            address = host.server_address()
            assert address is not None
            with tempfile.TemporaryFile(mode="w+", encoding="utf-8") as errors:
                with subprocess.Popen(
                    [artifacts.node_binary, str(script), address, first.url, second.url,
                     local_root, value.binary_bytes().hex(), first_route, second_route],
                    cwd=root, env=_process_environment(), stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE, stderr=errors, text=True,
                ) as node:
                    try:
                        for command in ("unregister", "register"):
                            request = readline_with_timeout(node.stdout, 30).strip()
                            if request != command:
                                errors.seek(0)
                                pytest.fail(f"Node sent {request!r}, expected {command!r}\n{errors.read()}")
                            if command == "unregister":
                                host.unregister(first_route)
                            else:
                                host.register(Telemetry, object(), name=first_route, initial_topics={"state": value})
                            node.stdin.write("ok\n")
                            node.stdin.flush()
                        node.stdin.close()
                        status = node.wait(timeout=30)
                        errors.seek(0)
                        assert status == 0, errors.read()
                    finally:
                        if node.poll() is None:
                            node.kill()
                            node.wait(timeout=5)
    finally:
        value.close()


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
        / "target/local-rc/typescript-real-call-receipt.v3.json"
    )
    receipt = {
        "schema": "c-two.typescript-real-call-receipt.v3",
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
