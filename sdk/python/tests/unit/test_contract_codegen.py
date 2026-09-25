from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

import pytest

import c_two as cc


FIXTURE = (
    Path(__file__).parents[4]
    / "tests"
    / "fixtures"
    / "contracts"
    / "portable-release.contract.json"
)


def test_python_codegen_projects_the_shared_rust_artifact_set() -> None:
    descriptor = FIXTURE.read_bytes()
    first = cc.compile_contract_artifacts(descriptor, target="python")
    second = cc.compile_contract_artifacts(descriptor, target="python")

    assert first == second
    assert first.total_bytes == sum(len(artifact.bytes) for artifact in first.artifacts)
    assert first.get("python/c_two_contract.py") is not None
    assert first.get("metadata/contract.json") is not None
    assert any("/payloads/" in artifact.relative_path for artifact in first.artifacts)
    assert tuple(artifact.relative_path for artifact in first.artifacts) == tuple(
        sorted(artifact.relative_path for artifact in first.artifacts)
    )
    for artifact in first.artifacts:
        assert hashlib.sha256(artifact.bytes).hexdigest() == artifact.sha256
        assert artifact.owner in {"c-two", "fastdb-core"}


def test_python_codegen_preserves_structured_fastdb_error_fields() -> None:
    descriptor = json.loads(FIXTURE.read_text())
    descriptor["methods"][1]["bindings"]["input"]["spec"] = {"schema": "wrong"}
    descriptor["methods"][1]["bindings"]["output"]["spec"] = {"schema": "wrong"}
    abi_hash, signature_hash = cc._native.derive_portable_contract_fingerprints(
        json.dumps(descriptor, separators=(",", ":")).encode()
    )
    descriptor["fingerprints"]["abi_hash"] = abi_hash
    descriptor["fingerprints"]["signature_hash"] = signature_hash

    with pytest.raises(cc.ContractCodegenError) as caught:
        cc.compile_contract_artifacts(
            json.dumps(descriptor, separators=(",", ":")).encode(),
            target="python",
        )

    error = caught.value
    assert error.binding_path == "$.methods[1].bindings.input.spec"
    assert isinstance(error.code, int) and error.code != 0
    assert isinstance(error.symbol, str) and error.symbol
    assert isinstance(error.path, str) and error.path
    assert isinstance(error.message, str) and error.message
    assert isinstance(json.loads(error.details_json), dict)
    assert error.profile is None
    assert error.metric is None
    assert error.limit is None
    assert error.observed is None


def test_python_codegen_non_fastdb_errors_have_explicit_empty_cause_fields() -> None:
    with pytest.raises(cc.ContractCodegenError) as caught:
        cc.compile_contract_artifacts(FIXTURE.read_bytes(), target="cpp")

    error = caught.value
    assert error.binding_path is None
    assert error.code is None
    assert error.symbol is None
    assert error.path is None
    assert error.details_json is None
    assert error.profile is None
    assert error.metric is None
    assert error.limit is None
    assert error.observed is None
    assert error.message == str(error)
    assert "unsupported contract codegen target" in error.message


def test_python_codegen_projects_typed_contract_admission_fields() -> None:
    descriptor = json.loads(FIXTURE.read_text())
    template = descriptor["methods"][0]
    descriptor["methods"] = [
        {**template, "name": f"method_{index:03}"}
        for index in range(257)
    ]

    with pytest.raises(cc.ContractCodegenError) as caught:
        cc.compile_contract_artifacts(
            json.dumps(descriptor, separators=(",", ":")).encode(),
            target="python",
        )

    error = caught.value
    assert error.binding_path is None
    assert error.code is None
    assert error.symbol is None
    assert error.details_json is None
    assert error.profile == "v1"
    assert error.metric == "methods"
    assert error.limit == 256
    assert error.observed == 257
    assert error.path == "$.methods"
    assert error.message == str(error)


def test_generated_python_project_imports_with_installed_fastdb_projection(
    tmp_path: Path,
) -> None:
    generated = cc.compile_contract_artifacts(FIXTURE.read_bytes(), target="python")
    for artifact in generated.artifacts:
        output = tmp_path / artifact.relative_path
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(artifact.bytes)

    generated_source = tmp_path / "python"
    environment = os.environ.copy()
    environment["PYTHONPATH"] = str(generated_source)
    completed = subprocess.run(
        [
            sys.executable,
            "-c",
            (
                "import c_two_contract as contract; "
                "assert contract.CONTRACT_SCHEMA == 'c-two.contract.v2'; "
                "assert [method.name for method in contract.METHODS] == ['ping', 'echo']"
            ),
        ],
        cwd=generated_source,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
    )
    assert completed.returncode == 0, completed.stderr


def test_generated_python_decoder_closes_owner_and_preserves_digest_error(
    tmp_path: Path,
) -> None:
    generated = cc.compile_contract_artifacts(FIXTURE.read_bytes(), target="python")
    for artifact in generated.artifacts:
        output = tmp_path / artifact.relative_path
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(artifact.bytes)

    stub = tmp_path / "stub" / "fastdb4py" / "payload"
    stub.mkdir(parents=True)
    (stub.parent / "__init__.py").write_text("", encoding="utf-8")
    (stub / "__init__.py").write_text(
        """
events = []


class PayloadError(Exception):
    pass


class CompiledSpec:
    @classmethod
    def compile(cls, _source):
        return cls()


class Payload:
    @classmethod
    def open_copy(cls, _spec, _data):
        events.append("open")
        return cls()

    def require_spec_sha256(self, _expected):
        events.append("require")
        raise RuntimeError("digest guard failed")

    def close(self):
        events.append("close")
        raise RuntimeError("close also failed")


class Builder:
    pass


class GraphIdentity:
    pass


class ObjectHandle:
    pass


class View:
    pass
""".lstrip(),
        encoding="utf-8",
    )
    generated_source = tmp_path / "python"
    environment = os.environ.copy()
    environment["PYTHONPATH"] = os.pathsep.join(
        [str(tmp_path / "stub"), str(generated_source)]
    )
    completed = subprocess.run(
        [
            sys.executable,
            "-c",
            (
                "from fastdb4py.payload import events; "
                "import c_two_contract as contract; "
                "\ntry:\n"
                "    contract.decode_method_1_echo_output(b'x')\n"
                "except RuntimeError as error:\n"
                "    assert str(error) == 'digest guard failed', error\n"
                "else:\n"
                "    raise AssertionError('digest guard unexpectedly succeeded')\n"
                "assert events == ['open', 'require', 'close'], events\n"
                "events.clear()\n"
                "class CopyAndReleaseFail:\n"
                "    def __bytes__(self):\n"
                "        raise RuntimeError('response copy failed')\n"
                "    def release(self):\n"
                "        events.append('response:release')\n"
                "        raise RuntimeError('response release failed')\n"
                "class CopySuccessReleaseFail(CopyAndReleaseFail):\n"
                "    def __bytes__(self):\n"
                "        return b''\n"
                "class BoundClient:\n"
                "    def __init__(self, response):\n"
                "        self.response = response\n"
                "    def call(self, _method_name, _data):\n"
                "        return self.response\n"
                "try:\n"
                "    contract.ContractClient(BoundClient(CopyAndReleaseFail())).method_0_ping()\n"
                "except RuntimeError as error:\n"
                "    assert str(error) == 'response copy failed', error\n"
                "else:\n"
                "    raise AssertionError('response copy unexpectedly succeeded')\n"
                "assert events == ['response:release'], events\n"
                "events.clear()\n"
                "try:\n"
                "    contract.ContractClient(BoundClient(CopySuccessReleaseFail())).method_0_ping()\n"
                "except RuntimeError as error:\n"
                "    assert str(error) == 'response release failed', error\n"
                "else:\n"
                "    raise AssertionError('response release failure was hidden')\n"
                "assert events == ['response:release'], events\n"
            ),
        ],
        cwd=generated_source,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
    )
    assert completed.returncode == 0, completed.stderr
