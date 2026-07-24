from __future__ import annotations

from contextlib import nullcontext
from copy import deepcopy
import os
from pathlib import Path
import tempfile
from typing import Any, Iterator

import pytest

import c_two as cc
from c_two.config.settings import settings
from c_two.error import (
    ContractMismatch,
    ResourceNotFound,
    ResourceRemoved,
    RouteStale,
)
from c_two.transport.registry import _ProcessRegistry
from fastdb4py.payload import Payload
from tests.fixtures.portable_interop import (
    GRAPH_SPEC,
    PortableNoPayload,
    PortableObjectGraph,
    PortableRecord,
    assert_record_detached,
    assert_view_invalidated,
    build_record_payload,
    inspect_record_payload,
    logical_result_sha256,
)
from tests.fixtures.portable_matrix import (
    CRM_CLASSES,
    MatrixArtifacts,
    MatrixRelay,
    RustHost,
    configure_python_runtime,
    input_lifetime_for,
    invoke_generated_python,
    resource_for,
    run_rust_client,
    wait_for_contract_resolution,
)


REPOSITORY = Path(__file__).resolve().parents[4]
import sys

sys.path.insert(0, str(REPOSITORY))

from tools.local_rc.portable_matrix_receipt import (  # noqa: E402
    EXPECTED_ROW_IDS,
    load_and_validate_receipt,
    write_receipt,
)


pytestmark = pytest.mark.timeout(300)
_MATRIX_ROWS: dict[str, dict[str, Any]] = {}


@pytest.fixture(scope="module")
def matrix_artifacts() -> Iterator[MatrixArtifacts]:
    with tempfile.TemporaryDirectory(
        prefix="c-two-portable-matrix-",
    ) as directory:
        yield MatrixArtifacts.prepare(Path(directory))


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
    compact = (
        row_id.replace("object-graph-v1", "graph")
        .replace("no-payload", "none")
        .replace("record-v1", "record")
        .replace("__", "-")
    )
    return f"matrix-{compact}-{os.getpid()}"


def _ipc_address(row_id: str) -> str:
    digest = __import__("hashlib").sha256(row_id.encode("utf-8")).hexdigest()[:16]
    return f"ipc://c2_matrix_{os.getpid()}_{digest}"


def _assert_client_receipt(
    receipt: dict[str, str],
    *,
    expected_path: str,
) -> tuple[str, int, int, int]:
    assert receipt["observed_path"] == expected_path
    route_uid = receipt["route_uid"]
    route_revision = int(receipt["route_revision"])
    requests = int(receipt["requests"])
    responses = int(receipt["responses"])
    assert route_uid
    assert route_revision > 0
    assert requests > 0
    assert responses > 0
    return route_uid, route_revision, requests, responses


def _run_python_host_row(
    artifacts: MatrixArtifacts,
    *,
    payload: str,
    transport: str,
    route_name: str,
    relay_url: str | None,
) -> tuple[str, int, int, int]:
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
    actual_address = cc.server_address()
    assert actual_address is not None
    try:
        resolved = (
            wait_for_contract_resolution(
                relay_url,
                route_name,
                artifacts.generated_python[payload],
            )
            if relay_url is not None
            else None
        )
        receipt = run_rust_client(
            artifacts,
            payload=payload,
            transport=transport,
            endpoint=actual_address if transport == "direct" else relay_url or "",
            route_name=route_name,
        )
        assert resource.calls == 1
        resource.assert_lifetime()
        result = _assert_client_receipt(
            receipt,
            expected_path="DirectIpc" if transport == "direct" else "ExplicitRelay",
        )
        if resolved is not None:
            assert resolved["route_uid"] == result[0]
            assert resolved["route_revision"] == result[1]
        return result
    finally:
        _ProcessRegistry.reset()
        resource.close()


def _run_rust_host_row(
    artifacts: MatrixArtifacts,
    *,
    payload: str,
    client_language: str,
    transport: str,
    route_name: str,
    address: str,
    relay_url: str | None,
) -> tuple[str, int, int, int]:
    host = RustHost(
        artifacts,
        payload=payload,
        address=address,
        route_name=route_name,
        relay_url=relay_url,
    )
    result: tuple[str, int, int, int] | None = None
    try:
        resolved = (
            wait_for_contract_resolution(
                relay_url,
                route_name,
                artifacts.generated_python[payload],
            )
            if relay_url is not None
            else None
        )
        if resolved is not None:
            assert resolved["route_uid"] == host.route_uid
            assert resolved["route_revision"] == host.route_revision
        endpoint = address if transport == "direct" else relay_url or ""
        if client_language == "rust":
            receipt = run_rust_client(
                artifacts,
                payload=payload,
                transport=transport,
                endpoint=endpoint,
                route_name=route_name,
            )
            result = _assert_client_receipt(
                receipt,
                expected_path=(
                    "DirectIpc" if transport == "direct" else "ExplicitRelay"
                ),
            )
        else:
            configure_python_runtime(
                server=False,
                client=True,
                relay_url=None,
            )
            proxy = cc.connect(
                CRM_CLASSES[payload],
                name=route_name,
                address=endpoint,
            )
            try:
                invoke_generated_python(
                    artifacts.generated_python[payload],
                    proxy,
                    payload,
                )
                expected_path = (
                    "direct_ipc" if transport == "direct" else "explicit_relay"
                )
                assert proxy.client.observed_path == expected_path
                result = (
                    proxy.client.route_uid,
                    proxy.client.route_revision,
                    1,
                    1,
                )
            finally:
                cc.close(proxy)
                _ProcessRegistry.reset()
    finally:
        host_calls = host.stop()
    assert host_calls == 1
    assert result is not None
    assert result[0] == host.route_uid
    assert result[1] == host.route_revision
    return result


@pytest.mark.parametrize("row_id", EXPECTED_ROW_IDS, ids=EXPECTED_ROW_IDS)
def test_portable_payload_matrix_row(
    row_id: str,
    matrix_artifacts: MatrixArtifacts,
) -> None:
    payload, client_role, host_role, transport = row_id.split("__")
    client_language = client_role.removesuffix("-client")
    host_language = host_role.removesuffix("-host")
    route_name = _route_name(row_id)
    address = _ipc_address(row_id)
    relay_context = (
        MatrixRelay(matrix_artifacts, row_id)
        if transport == "relay"
        else nullcontext(None)
    )

    with relay_context as relay:
        relay_url = relay.url if relay is not None else None
        if host_language == "python":
            route_uid, route_revision, requests, responses = (
                _run_python_host_row(
                    matrix_artifacts,
                    payload=payload,
                    transport=transport,
                    route_name=route_name,
                    relay_url=relay_url,
                )
            )
        else:
            route_uid, route_revision, requests, responses = (
                _run_rust_host_row(
                    matrix_artifacts,
                    payload=payload,
                    client_language=client_language,
                    transport=transport,
                    route_name=route_name,
                    address=address,
                    relay_url=relay_url,
                )
            )

    facts = matrix_artifacts.contracts[payload]
    _MATRIX_ROWS[row_id] = {
        "id": row_id,
        "payload": payload,
        "client_language": client_language,
        "host_language": host_language,
        "transport": transport,
        "observed_path": (
            "DirectIpc" if transport == "direct" else "ExplicitRelay"
        ),
        "descriptor_sha256": facts.descriptor_sha256,
        "contract_release_ref": deepcopy(facts.release_ref),
        "fastdb_spec_sha256": facts.fastdb_spec_sha256,
        "route": {
            "uid": route_uid,
            "revision": route_revision,
        },
        "logical_result_sha256": logical_result_sha256(payload),
        "packages": {
            "client_sha256": matrix_artifacts.package_sha256(client_language),
            "host_sha256": matrix_artifacts.package_sha256(host_language),
        },
        "c3_sha256": (
            matrix_artifacts.c3_sha256 if transport == "relay" else None
        ),
        "path_counters": {
            "requests": requests,
            "responses": responses,
        },
        "status": "passed",
    }


def test_portable_payload_matrix_writes_complete_receipt(
    matrix_artifacts: MatrixArtifacts,
) -> None:
    assert tuple(_MATRIX_ROWS) == EXPECTED_ROW_IDS
    evidence_stage = os.environ.get(
        "C2_PORTABLE_MATRIX_EVIDENCE_STAGE",
        "development",
    )
    configured_path = os.environ.get("C2_PORTABLE_MATRIX_RECEIPT")
    receipt_path = (
        Path(configured_path)
        if configured_path
        else REPOSITORY
        / "target/local-rc/portable-matrix-receipt.v1.json"
    )
    receipt = {
        "schema": "c-two.portable-matrix-receipt.v1",
        "evidence_stage": evidence_stage,
        "rows": [_MATRIX_ROWS[row_id] for row_id in EXPECTED_ROW_IDS],
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
    assert matrix_artifacts.rust_harness.is_file()


@pytest.mark.parametrize("transport", ("direct", "relay"))
def test_route_disappearance_preserves_release_reference(
    transport: str,
    matrix_artifacts: MatrixArtifacts,
) -> None:
    row_id = f"route-disappearance-{transport}"
    relay_context = (
        MatrixRelay(matrix_artifacts, row_id)
        if transport == "relay"
        else nullcontext(None)
    )
    with relay_context as relay:
        relay_url = relay.url if relay is not None else None
        configure_python_runtime(
            server=True,
            client=True,
            relay_url=relay_url,
        )
        resource = resource_for("no-payload")
        route_name = f"matrix-disappearance-{transport}-{os.getpid()}"
        cc.register(PortableNoPayload, resource, name=route_name)
        address = cc.server_address()
        assert address is not None
        endpoint = address if transport == "direct" else relay_url
        assert endpoint is not None
        retained_ref = cc.export_contract_release_ref(PortableNoPayload)
        proxy = cc.connect(
            PortableNoPayload,
            name=route_name,
            address=endpoint,
        )
        try:
            assert proxy.ping() is None
            retained_route = (
                proxy.client.route_uid,
                proxy.client.route_revision,
            )
            cc.unregister(route_name)
            expected_error = (
                ResourceRemoved
                if transport == "direct"
                else (ResourceNotFound, RouteStale)
            )
            with pytest.raises(expected_error):
                proxy.ping()
            assert cc.export_contract_release_ref(PortableNoPayload) == retained_ref
            assert retained_route[0]
            assert retained_route[1] > 0
        finally:
            cc.close(proxy)
            _ProcessRegistry.reset()
            resource.close()


@pytest.mark.parametrize("transport", ("direct", "relay"))
def test_contract_fingerprint_mismatch_is_transport_scoped(
    transport: str,
    matrix_artifacts: MatrixArtifacts,
) -> None:
    def wrong_roundtrip(self: object, payload: Payload) -> Payload:
        del self
        return payload

    wrong_roundtrip = cc.transfer(
        input=GRAPH_SPEC,
        output=GRAPH_SPEC,
    )(wrong_roundtrip)
    wrong_contract = cc.crm(
        namespace="test.portable-matrix.record-v1",
        version="0.1.0",
    )(
        type(
            "PortableRecord",
            (),
            {"roundtrip": wrong_roundtrip},
        )
    )

    row_id = f"fingerprint-mismatch-{transport}"
    relay_context = (
        MatrixRelay(matrix_artifacts, row_id)
        if transport == "relay"
        else nullcontext(None)
    )
    with relay_context as relay:
        relay_url = relay.url if relay is not None else None
        configure_python_runtime(
            server=True,
            client=True,
            relay_url=relay_url,
        )
        resource = resource_for("record-v1")
        route_name = f"matrix-fingerprint-{transport}-{os.getpid()}"
        cc.register(
            PortableRecord,
            resource,
            name=route_name,
            input_lifetime=input_lifetime_for("record-v1"),
        )
        address = cc.server_address()
        assert address is not None
        endpoint = address if transport == "direct" else relay_url
        assert endpoint is not None
        expected_error = ContractMismatch if transport == "direct" else ResourceNotFound
        try:
            with pytest.raises(expected_error):
                cc.connect(
                    wrong_contract,
                    name=route_name,
                    address=endpoint,
                )
            assert resource.calls == 0
        finally:
            _ProcessRegistry.reset()
            resource.close()


def test_held_response_invalidates_before_relay_lease_release(
    matrix_artifacts: MatrixArtifacts,
) -> None:
    with MatrixRelay(matrix_artifacts, "held-response-relay") as relay:
        configure_python_runtime(
            server=True,
            client=True,
            relay_url=relay.url,
        )
        resource = resource_for("record-v1")
        route_name = f"matrix-held-relay-{os.getpid()}"
        cc.register(
            PortableRecord,
            resource,
            name=route_name,
            input_lifetime=input_lifetime_for("record-v1"),
        )
        proxy = cc.connect(
            PortableRecord,
            name=route_name,
            address=relay.url,
        )
        source = build_record_payload()
        retained: Payload | None = None
        root = None
        detached = None
        try:
            with cc.hold(proxy.roundtrip)(source) as held:
                retained = held.value
                root, detached = inspect_record_payload(retained)
                assert cc.hold_stats()["active_holds"] == 1
            assert root is not None
            assert detached is not None
            assert_view_invalidated(root)
            assert_record_detached(detached)
            assert cc.hold_stats()["active_holds"] == 0
            resource.assert_lifetime()
        finally:
            for value in (root, detached, retained, source):
                if value is not None:
                    value.close()
            cc.close(proxy)
            _ProcessRegistry.reset()
            resource.close()
