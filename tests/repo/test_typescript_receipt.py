from __future__ import annotations

from copy import deepcopy
import json
from pathlib import Path
import sys

import pytest


REPOSITORY = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPOSITORY))

from tools.local_rc.typescript_receipt import (  # noqa: E402
    DIGEST_ROW_ID,
    EXPECTED_ROW_IDS,
    MODES,
    OPAQUE_ROW_ID,
    PAYLOADS,
    ReceiptValidationError,
    load_and_validate_receipt,
    validate_receipt,
    write_receipt,
)


def _digest(value: str) -> str:
    return value * 64


def _receipt() -> dict[str, object]:
    rows = []
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
    for index, row_id in enumerate(EXPECTED_ROW_IDS, start=1):
        payload, mode = row_id.split("__")
        rows.append(
            {
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
                    "crm": {
                        "namespace": f"test.typescript.{payload}",
                        "name": "Portable",
                        "version": "0.1.0",
                    },
                },
                "fastdb_spec_sha256": (
                    None if payload == "no-payload" else _digest("b")
                ),
                "route": {"uid": f"typescript-route-{index}", "revision": 1},
                "logical_result_sha256": _digest("c"),
                "path_counters": {
                    "requests": 2 if row_id == OPAQUE_ROW_ID else 1,
                    "responses": 2 if row_id == OPAQUE_ROW_ID else 1,
                },
                "lifetime": {
                    "close_idempotent": True,
                    "checked_view_invalidated": (
                        None if payload == "no-payload" else True
                    ),
                    "materialized_survived": (
                        None if payload == "no-payload" else True
                    ),
                },
                "status": "passed",
            }
        )
    return {
        "schema": "c-two.typescript-real-call-receipt.v1",
        "evidence_stage": "development",
        "runtime": {
            "node_version": "v25.8.1",
            "platform": "darwin",
            "arch": "arm64",
            "browser_runtime": "unverified",
        },
        "packages": {
            "fastdb4ts_sha256": _digest("d"),
            "c2_mem_ffi_sha256": _digest("e"),
            "c3_sha256": _digest("f"),
        },
        "rows": rows,
        "negative_evidence": {
            "digest_mismatch": {
                "verified": True,
                "row_id": DIGEST_ROW_ID,
                "fields": {
                    "cause_owner": "fastdb",
                    "fastdb_code": "3006",
                    "fastdb_details_json": (
                        '{"actual":"1a6444b96fce6f7be651b3c865db574563f287c3ac9144af71b20b6a517cfb3a",'
                        '"expected":"7926cb8bda4c35f3cddcc28e6f9105331d7e46864ad1eb7bf03abedbcd8c7d8c",'
                        '"reason":"spec_digest_mismatch"}'
                    ),
                    "fastdb_message": "Portable payload spec digest does not match",
                    "fastdb_path": "/payload/spec_sha256",
                    "fastdb_symbol": "DIGEST_MISMATCH",
                },
            },
            "opaque_allocator": {
                "verified": True,
                "released": 1,
                "row_id": OPAQUE_ROW_ID,
            },
        },
        "cleanup": {
            "host_processes_stopped": True,
            "relay_processes_stopped": True,
            "node_processes_stopped": True,
            "response_readers_closed": True,
        },
    }


def test_dimensions_freeze_all_real_node_paths() -> None:
    assert PAYLOADS == ("no-payload", "record-v1", "object-graph-v1")
    assert MODES == (
        "direct-ipc",
        "explicit-relay",
        "relay-aware-local-ipc",
        "relay-aware-http",
    )
    assert len(EXPECTED_ROW_IDS) == 12


def test_receipt_round_trips_as_canonical_json(tmp_path: Path) -> None:
    receipt = _receipt()
    destination = tmp_path / "typescript-real-call-receipt.v1.json"

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
        (lambda receipt: receipt["rows"].pop(), "missing row"),
        (
            lambda receipt: receipt["rows"].append(
                deepcopy(receipt["rows"][0]),
            ),
            "duplicate row",
        ),
        (
            lambda receipt: receipt["rows"][0].__setitem__(
                "observed_path",
                "ExplicitRelay",
            ),
            "observed_path",
        ),
        (
            lambda receipt: receipt["runtime"].__setitem__(
                "browser_runtime",
                "verified",
            ),
            "browser_runtime",
        ),
        (
            lambda receipt: receipt["negative_evidence"][
                "opaque_allocator"
            ].__setitem__("released", 0),
            "released",
        ),
        (
            lambda receipt: receipt["negative_evidence"][
                "digest_mismatch"
            ]["fields"].__setitem__("fastdb_symbol", "OTHER"),
            "frozen FastDB digest mismatch cause",
        ),
        (
            lambda receipt: receipt["negative_evidence"][
                "digest_mismatch"
            ].__setitem__("row_id", OPAQUE_ROW_ID),
            "digest_mismatch.row_id",
        ),
        (
            lambda receipt: receipt["negative_evidence"][
                "opaque_allocator"
            ].__setitem__("row_id", DIGEST_ROW_ID),
            "opaque_allocator.row_id",
        ),
        (
            lambda receipt: receipt["rows"][0].__setitem__(
                "host_language",
                "python",
            ),
            "host_language",
        ),
        (
            lambda receipt: receipt["rows"][0]["path_counters"].__setitem__(
                "requests",
                2,
            ),
            "path_counters.requests",
        ),
        (
            lambda receipt: receipt["cleanup"].__setitem__(
                "node_processes_stopped",
                False,
            ),
            "node_processes_stopped",
        ),
    ],
)
def test_incomplete_or_overclaimed_evidence_is_rejected(
    mutate,
    message: str,
) -> None:
    receipt = _receipt()
    mutate(receipt)

    with pytest.raises(ReceiptValidationError, match=message):
        validate_receipt(receipt, expected_stage="development")


def test_absolute_paths_and_duplicate_json_keys_are_rejected(
    tmp_path: Path,
) -> None:
    receipt = _receipt()
    receipt["runtime"]["platform"] = "/tmp/not-portable"
    with pytest.raises(ReceiptValidationError, match="absolute path"):
        validate_receipt(receipt, expected_stage="development")

    destination = tmp_path / "duplicate.json"
    destination.write_text(
        json.dumps(_receipt()).replace(
            '"evidence_stage": "development"',
            '"evidence_stage": "development", "evidence_stage": "development"',
            1,
        ),
        encoding="utf-8",
    )
    with pytest.raises(ReceiptValidationError, match="duplicate JSON key"):
        load_and_validate_receipt(destination, expected_stage="development")
