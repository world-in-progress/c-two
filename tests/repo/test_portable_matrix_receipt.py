from __future__ import annotations

from copy import deepcopy
import json
from pathlib import Path
import sys
from typing import Any, Callable

import pytest

REPOSITORY = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPOSITORY))

from tools.local_rc.portable_matrix_receipt import (
    DIRECTIONS,
    EXPECTED_ROW_IDS,
    PAYLOADS,
    TRANSPORTS,
    ReceiptValidationError,
    load_and_validate_receipt,
    validate_receipt,
    write_receipt,
)


EXPECTED_IDS = (
    "no-payload__rust-client__rust-host__direct",
    "no-payload__rust-client__rust-host__relay",
    "no-payload__rust-client__python-host__direct",
    "no-payload__rust-client__python-host__relay",
    "no-payload__python-client__rust-host__direct",
    "no-payload__python-client__rust-host__relay",
    "record-v1__rust-client__rust-host__direct",
    "record-v1__rust-client__rust-host__relay",
    "record-v1__rust-client__python-host__direct",
    "record-v1__rust-client__python-host__relay",
    "record-v1__python-client__rust-host__direct",
    "record-v1__python-client__rust-host__relay",
    "object-graph-v1__rust-client__rust-host__direct",
    "object-graph-v1__rust-client__rust-host__relay",
    "object-graph-v1__rust-client__python-host__direct",
    "object-graph-v1__rust-client__python-host__relay",
    "object-graph-v1__python-client__rust-host__direct",
    "object-graph-v1__python-client__rust-host__relay",
)


def _digest(character: str) -> str:
    return character * 64


def _valid_receipt() -> dict[str, Any]:
    rows: list[dict[str, Any]] = []
    for index, row_id in enumerate(EXPECTED_IDS, start=1):
        payload, client_role, host_role, transport = row_id.split("__")
        descriptor_sha256 = _digest("a")
        rows.append(
            {
                "id": row_id,
                "payload": payload,
                "client_language": client_role.removesuffix("-client"),
                "host_language": host_role.removesuffix("-host"),
                "transport": transport,
                "observed_path": (
                    "DirectIpc" if transport == "direct" else "ExplicitRelay"
                ),
                "descriptor_sha256": descriptor_sha256,
                "contract_release_ref": {
                    "schema": "c-two.contract-release-ref.v1",
                    "contract_schema": "c-two.contract.v2",
                    "descriptor_sha256": descriptor_sha256,
                    "crm": {
                        "namespace": f"test.portable-matrix.{payload}",
                        "name": "PortableMatrix",
                        "version": "0.1.0",
                    },
                },
                "fastdb_spec_sha256": (
                    None if payload == "no-payload" else _digest("b")
                ),
                "route": {
                    "uid": f"matrix-route-{index:02d}",
                    "revision": 1,
                },
                "logical_result_sha256": _digest("c"),
                "packages": {
                    "client_sha256": _digest("d"),
                    "host_sha256": _digest("e"),
                },
                "c3_sha256": None if transport == "direct" else _digest("f"),
                "path_counters": {
                    "requests": 1,
                    "responses": 1,
                },
                "status": "passed",
            }
        )
    return {
        "schema": "c-two.portable-matrix-receipt.v1",
        "evidence_stage": "development",
        "rows": rows,
    }


def test_dimensions_and_exact_order_are_frozen() -> None:
    assert PAYLOADS == ("no-payload", "record-v1", "object-graph-v1")
    assert DIRECTIONS == (
        "rust-client__rust-host",
        "rust-client__python-host",
        "python-client__rust-host",
    )
    assert TRANSPORTS == ("direct", "relay")
    assert EXPECTED_ROW_IDS == EXPECTED_IDS


def test_valid_receipt_round_trips_as_canonical_json(tmp_path: Path) -> None:
    receipt = _valid_receipt()
    destination = tmp_path / "portable-matrix-receipt.v1.json"

    write_receipt(destination, receipt, expected_stage="development")
    first = destination.read_bytes()
    write_receipt(destination, receipt, expected_stage="development")

    assert destination.read_bytes() == first
    assert first.endswith(b"\n")
    assert load_and_validate_receipt(
        destination,
        expected_stage="development",
    ) == receipt


@pytest.mark.parametrize(
    ("mutate", "message"),
    [
        (lambda receipt: receipt["rows"].pop(0), "missing row"),
        (
            lambda receipt: receipt["rows"].append(deepcopy(receipt["rows"][0])),
            "duplicate row",
        ),
        (
            lambda receipt: receipt["rows"][-1].__setitem__(
                "id",
                "record-v1__python-client__python-host__direct",
            ),
            "unexpected row",
        ),
        (
            lambda receipt: receipt["rows"].__setitem__(
                slice(0, 2),
                [receipt["rows"][1], receipt["rows"][0]],
            ),
            "stable.*order",
        ),
    ],
)
def test_row_set_rejects_missing_duplicate_unexpected_and_reordered_rows(
    mutate: Callable[[dict[str, Any]], object],
    message: str,
) -> None:
    receipt = _valid_receipt()
    mutate(receipt)

    with pytest.raises(ReceiptValidationError, match=message):
        validate_receipt(receipt, expected_stage="development")


@pytest.mark.parametrize("status", ["skipped", "xfailed", "failed", "error"])
def test_every_row_must_be_passing(status: str) -> None:
    receipt = _valid_receipt()
    receipt["rows"][4]["status"] = status

    with pytest.raises(ReceiptValidationError, match="status must be 'passed'"):
        validate_receipt(receipt, expected_stage="development")


@pytest.mark.parametrize(
    ("field", "value", "message"),
    [
        ("observed_path", "RelayAwareLocalIpc", "ExplicitRelay"),
        ("request_count", 0, "positive integer"),
        ("response_count", 0, "positive integer"),
    ],
)
def test_relay_rows_require_explicit_path_and_positive_counters(
    field: str,
    value: object,
    message: str,
) -> None:
    receipt = _valid_receipt()
    relay_row = receipt["rows"][1]
    if field == "request_count":
        relay_row["path_counters"]["requests"] = value
    elif field == "response_count":
        relay_row["path_counters"]["responses"] = value
    else:
        relay_row[field] = value

    with pytest.raises(ReceiptValidationError, match=message):
        validate_receipt(receipt, expected_stage="development")


@pytest.mark.parametrize(
    "mutate",
    [
        lambda receipt: receipt.__setitem__("unexpected", True),
        lambda receipt: receipt["rows"][0].__setitem__("unexpected", True),
        lambda receipt: receipt["rows"][0]["route"].__setitem__(
            "unexpected",
            True,
        ),
        lambda receipt: receipt["rows"][0]["contract_release_ref"]["crm"].__setitem__(
            "unexpected",
            True,
        ),
    ],
)
def test_unknown_fields_are_rejected(
    mutate: Callable[[dict[str, Any]], object],
) -> None:
    receipt = _valid_receipt()
    mutate(receipt)

    with pytest.raises(ReceiptValidationError, match="unknown field"):
        validate_receipt(receipt, expected_stage="development")


def test_absolute_paths_are_rejected() -> None:
    receipt = _valid_receipt()
    receipt["rows"][0]["route"]["uid"] = "/tmp/not-portable"

    with pytest.raises(ReceiptValidationError, match="absolute path"):
        validate_receipt(receipt, expected_stage="development")


def test_duplicate_json_keys_are_rejected(tmp_path: Path) -> None:
    destination = tmp_path / "duplicate.json"
    destination.write_text(
        '{"schema":"c-two.portable-matrix-receipt.v1",'
        '"schema":"c-two.portable-matrix-receipt.v1",'
        '"evidence_stage":"development","rows":[]}',
        encoding="utf-8",
    )

    with pytest.raises(ReceiptValidationError, match="duplicate JSON key"):
        load_and_validate_receipt(destination, expected_stage="development")


@pytest.mark.parametrize(
    ("row_index", "value", "message"),
    [
        (0, _digest("b"), "no-payload"),
        (6, None, "record-v1"),
        (12, None, "object-graph-v1"),
    ],
)
def test_fastdb_digest_presence_matches_payload_profile(
    row_index: int,
    value: str | None,
    message: str,
) -> None:
    receipt = _valid_receipt()
    receipt["rows"][row_index]["fastdb_spec_sha256"] = value

    with pytest.raises(ReceiptValidationError, match=message):
        validate_receipt(receipt, expected_stage="development")


def test_release_reference_must_match_descriptor_digest() -> None:
    receipt = _valid_receipt()
    receipt["rows"][0]["contract_release_ref"]["descriptor_sha256"] = _digest("0")

    with pytest.raises(ReceiptValidationError, match="descriptor_sha256"):
        validate_receipt(receipt, expected_stage="development")


def test_direct_rows_reject_relay_hash_and_wrong_observed_path() -> None:
    receipt = _valid_receipt()
    receipt["rows"][0]["c3_sha256"] = _digest("f")

    with pytest.raises(ReceiptValidationError, match="direct row"):
        validate_receipt(receipt, expected_stage="development")

    receipt = _valid_receipt()
    receipt["rows"][0]["observed_path"] = "ExplicitRelay"

    with pytest.raises(ReceiptValidationError, match="direct row"):
        validate_receipt(receipt, expected_stage="development")


def test_expected_evidence_stage_is_enforced() -> None:
    receipt = _valid_receipt()

    with pytest.raises(ReceiptValidationError, match="evidence_stage"):
        validate_receipt(receipt, expected_stage="candidate")


def test_raw_json_is_standard_json_not_nan(tmp_path: Path) -> None:
    destination = tmp_path / "nan.json"
    destination.write_text(
        json.dumps(_valid_receipt()).replace('"revision": 1', '"revision": NaN', 1),
        encoding="utf-8",
    )

    with pytest.raises(ReceiptValidationError, match="non-finite JSON number"):
        load_and_validate_receipt(destination, expected_stage="development")
