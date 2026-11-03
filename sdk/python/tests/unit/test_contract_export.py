from __future__ import annotations

import io
import json
from pathlib import Path
import sys

import pytest

import c_two as cc

import c_two.contract
from fastdb4py.payload import Payload


SPEC = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [
        {
            "id": "value",
            "cardinality": "one",
            "type": {"kind": "str", "nullable": True},
        }
    ],
    "components": [],
}


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[4]


@pytest.fixture
def unicode_contract(monkeypatch):
    @cc.crm(namespace="测试.契约🙂", version="0.1.0")
    class 问候:
        def ping(self) -> None:
            ...

    monkeypatch.setattr(sys.modules[__name__], "UnicodeContract", 问候, raising=False)
    return f"{__name__}:UnicodeContract", cc.contract.export_descriptor(问候)


def test_portable_export_rejects_python_pickle() -> None:
    @cc.crm(namespace="test.contract-export", version="0.1.0")
    class PythonOnly:
        def echo(self, value: bytes) -> bytes:
            ...

    with pytest.raises(ValueError, match="python-pickle-default"):
        cc.contract.export_descriptor(PythonOnly)


def test_contract_diagnostics_report_only_python_runtime_fallbacks() -> None:
    @cc.crm(namespace="test.contract-diagnostics", version="0.1.0")
    class PythonOnly:
        def echo(self, value: bytes) -> bytes:
            ...

    assert [
        (item["method"], item["position"], item["code"])
        for item in cc.contract.diagnostics(PythonOnly)
    ] == [
        ("echo", "input", "python_only_pickle"),
        ("echo", "output", "python_only_pickle"),
    ]


def test_explicit_fastdb_contract_has_no_python_diagnostics() -> None:
    @cc.crm(namespace="test.contract-diagnostics", version="0.1.0")
    class Portable:
        @cc.transfer(input=SPEC, output=SPEC)
        def echo(self, payload: Payload) -> Payload:
            ...

    assert cc.contract.diagnostics(Portable) == []


def test_export_descriptor_returns_canonical_v2_json() -> None:
    @cc.crm(namespace="test.contract-export", version="0.1.0")
    class Portable:
        @cc.transfer(input=SPEC, output=SPEC)
        def echo(self, payload: Payload) -> Payload:
            ...

    exported = cc.contract.export_descriptor(Portable)
    descriptor = json.loads(exported)
    from c_two.crm.contract import crm_contract

    contract = crm_contract(Portable)
    assert exported == json.dumps(
        descriptor,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    )
    assert descriptor["schema"] == "c-two.contract.v2"
    assert descriptor["fingerprints"] == {
        "abi_hash": contract.abi_hash,
        "signature_hash": contract.signature_hash,
    }
    assert descriptor["methods"][0]["bindings"]["input"]["spec"] == SPEC
    assert descriptor["methods"][0]["bindings"]["output"]["spec"] == SPEC
    assert "python-pickle-default" not in exported
    assert "call-db" not in exported


def test_native_release_projection_matches_shared_golden_vectors() -> None:
    from c_two._native import (
        canonicalize_portable_contract_descriptor,
        contract_release_ref_json,
        derive_portable_contract_fingerprints,
    )

    fixture_dir = _repo_root() / "tests" / "fixtures" / "contracts"
    descriptor = (fixture_dir / "portable-release.contract.json").read_bytes()
    canonical = (
        fixture_dir / "portable-release.canonical.json"
    ).read_text().strip()
    reference = (fixture_dir / "portable-release.ref.json").read_text().strip()

    assert canonicalize_portable_contract_descriptor(descriptor) == canonical
    assert contract_release_ref_json(descriptor) == reference
    assert derive_portable_contract_fingerprints(descriptor) == (
        "bec2fee73f9a2476c311de20e40e584d0c7ff6bcadd38c0b4fe3e683ec2540fe",
        "c4cd2cf04caa63f12f702868524787c95c4f5bc00a7c7212bd1f807b32647940",
    )


def test_release_ref_is_rust_derived_and_route_independent() -> None:
    @cc.crm(namespace="test.release-export", version="0.1.0")
    class Portable:
        @cc.transfer(input=SPEC, output=SPEC)
        def echo(self, payload: Payload) -> Payload:
            ...

    descriptor = cc.contract.export_descriptor(Portable)
    reference = json.loads(cc.contract.release_ref(Portable))
    from c_two._native import contract_release_ref_json

    assert reference == json.loads(contract_release_ref_json(descriptor.encode()))
    assert reference["schema"] == "c-two.contract-release-ref.v1"
    assert reference["contract_schema"] == "c-two.contract.v2"
    assert reference["crm"] == {
        "name": "Portable",
        "namespace": "test.release-export",
        "version": "0.1.0",
    }
    assert "route_name" not in reference
    assert "abi_hash" not in reference
    assert "signature_hash" not in reference


def test_pretty_descriptor_and_reference_preserve_release_identity() -> None:
    @cc.crm(namespace="test.release-pretty", version="0.1.0")
    class Ping:
        def ping(self) -> None:
            ...

    compact_descriptor = cc.contract.export_descriptor(Ping)
    pretty_descriptor = cc.contract.export_descriptor(Ping, pretty=True)
    compact_reference = cc.contract.release_ref(Ping)
    pretty_reference = cc.contract.release_ref(Ping, pretty=True)
    from c_two._native import contract_release_ref_json

    assert json.loads(compact_descriptor) == json.loads(pretty_descriptor)
    assert json.loads(compact_reference) == json.loads(pretty_reference)
    assert contract_release_ref_json(pretty_descriptor.encode()) == compact_reference


def test_contract_export_cli_writes_v2_descriptor(tmp_path, monkeypatch) -> None:
    module_path = tmp_path / "portable_contract_module.py"
    module_path.write_text(
        "\n".join(
            [
                "import c_two as cc; import c_two.contract",
                '@cc.crm(namespace="test.contract-export-cli", version="0.1.0")',
                "class Ping:",
                "    def ping(self) -> None:",
                "        ...",
                "",
            ],
        ),
    )
    out_path = tmp_path / "contract.json"
    monkeypatch.syspath_prepend(str(tmp_path))

    from c_two.cli.contract import main

    assert main(
        ["export", "portable_contract_module:Ping", "--out", str(out_path)],
    ) == 0
    descriptor = json.loads(out_path.read_text())

    assert descriptor["schema"] == "c-two.contract.v2"
    assert descriptor["crm"]["name"] == "Ping"
    assert descriptor["methods"][0]["name"] == "ping"


def test_contract_export_cli_writes_non_ascii_utf8(tmp_path, unicode_contract) -> None:
    from c_two.cli.contract import main

    target, payload = unicode_contract
    out_path = tmp_path / "契约.json"
    assert main(["export", target, "--out", str(out_path)]) == 0

    encoded = out_path.read_bytes()
    assert "测试.契约🙂" in payload
    assert encoded == payload.encode("utf-8")
    descriptor = json.loads(encoded.decode("utf-8"))
    assert descriptor["crm"]["name"] == "问候"
    assert descriptor["crm"]["namespace"] == "测试.契约🙂"


@pytest.mark.parametrize("encoding", ["cp1252", "gbk"])
def test_contract_export_cli_writes_utf8_stdout_in_order(
    monkeypatch, unicode_contract, encoding,
) -> None:
    from c_two.cli.contract import main

    target, payload = unicode_contract
    raw = io.BytesIO()
    with io.TextIOWrapper(io.BufferedWriter(raw), encoding=encoding, newline="\n") as stdout:
        with monkeypatch.context() as patch:
            patch.setattr(sys, "stdout", stdout)
            stdout.write("before\n")
            assert main(["export", target]) == 0
            encoded = raw.getvalue()

    assert encoded == b"before\n" + (payload + "\n").encode("utf-8")
    descriptor = json.loads(encoded.split(b"\n", 1)[1].decode("utf-8"))
    assert descriptor["crm"]["name"] == "问候"
    assert descriptor["crm"]["namespace"] == "测试.契约🙂"


def test_contract_diagnose_cli_writes_python_only_diagnostics(
    tmp_path,
    monkeypatch,
) -> None:
    module_path = tmp_path / "diagnostic_contract_module.py"
    module_path.write_text(
        "\n".join(
            [
                "import c_two as cc; import c_two.contract",
                '@cc.crm(namespace="test.contract-diagnose-cli", version="0.1.0")',
                "class PythonNative:",
                "    def echo(self, value: int) -> str:",
                "        ...",
                "",
            ],
        ),
    )
    out_path = tmp_path / "diagnostics.json"
    monkeypatch.syspath_prepend(str(tmp_path))

    from c_two.cli.contract import main

    assert main(
        [
            "diagnose",
            "diagnostic_contract_module:PythonNative",
            "--out",
            str(out_path),
        ],
    ) == 0
    diagnostics = json.loads(out_path.read_text())

    assert [
        (item["method"], item["position"], item["code"])
        for item in diagnostics
    ] == [
        ("echo", "input", "python_only_pickle"),
        ("echo", "output", "python_only_pickle"),
    ]
