from __future__ import annotations

from collections import Counter
import json
from pathlib import Path, PurePosixPath, PureWindowsPath
import re
from typing import Any, Mapping, NoReturn


PAYLOADS = ("no-payload", "record-v1", "object-graph-v1")
MODES = (
    "direct-ipc",
    "explicit-relay",
    "relay-aware-local-ipc",
    "relay-aware-http",
)
EXPECTED_ROW_IDS = tuple(
    f"{payload}__{mode}" for payload in PAYLOADS for mode in MODES
)
DIGEST_ROW_ID = "record-v1__direct-ipc"
OPAQUE_ROW_ID = "object-graph-v1__relay-aware-http"
RECEIPT_SCHEMA = "c-two.typescript-real-call-receipt.v1"
FROZEN_FASTDB_DIGEST_MISMATCH_CAUSE = {
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
}
_SHA256 = re.compile(r"^[0-9a-f]{64}$")
_ROOT_FIELDS = frozenset(
    (
        "schema",
        "evidence_stage",
        "runtime",
        "packages",
        "rows",
        "negative_evidence",
        "cleanup",
    )
)
_RUNTIME_FIELDS = frozenset(
    ("node_version", "platform", "arch", "browser_runtime")
)
_PACKAGE_FIELDS = frozenset(
    ("fastdb4ts_sha256", "c2_mem_ffi_sha256", "c3_sha256")
)
_ROW_FIELDS = frozenset(
    (
        "id",
        "payload",
        "mode",
        "host_language",
        "observed_path",
        "descriptor_sha256",
        "contract_release_ref",
        "fastdb_spec_sha256",
        "route",
        "logical_result_sha256",
        "path_counters",
        "lifetime",
        "status",
    )
)
_RELEASE_REF_FIELDS = frozenset(
    ("schema", "contract_schema", "descriptor_sha256", "crm")
)
_CRM_FIELDS = frozenset(("namespace", "name", "version"))
_ROUTE_FIELDS = frozenset(("uid", "revision"))
_COUNTER_FIELDS = frozenset(("requests", "responses"))
_LIFETIME_FIELDS = frozenset(
    ("close_idempotent", "checked_view_invalidated", "materialized_survived")
)
_NEGATIVE_FIELDS = frozenset(("digest_mismatch", "opaque_allocator"))
_DIGEST_FIELDS = frozenset(("verified", "row_id", "fields"))
_FASTDB_CAUSE_FIELDS = frozenset(
    (
        "cause_owner",
        "fastdb_code",
        "fastdb_details_json",
        "fastdb_message",
        "fastdb_path",
        "fastdb_symbol",
    )
)
_OPAQUE_FIELDS = frozenset(("verified", "released", "row_id"))
_CLEANUP_FIELDS = frozenset(
    (
        "host_processes_stopped",
        "relay_processes_stopped",
        "node_processes_stopped",
        "response_readers_closed",
    )
)
_OBSERVED_PATH = {
    "direct-ipc": "DirectIpc",
    "explicit-relay": "ExplicitRelay",
    "relay-aware-local-ipc": "RelayAwareLocalIpc",
    "relay-aware-http": "RelayAwareHttp",
}
_HOST_LANGUAGE = {
    "direct-ipc": "rust",
    "explicit-relay": "python",
    "relay-aware-local-ipc": "python",
    "relay-aware-http": "rust",
}


class ReceiptValidationError(ValueError):
    """TypeScript real-call evidence is incomplete, ambiguous, or nonportable."""


def _fail(path: str, message: str) -> NoReturn:
    raise ReceiptValidationError(f"{path}: {message}")


def _object(value: object, path: str) -> Mapping[str, Any]:
    if not isinstance(value, dict):
        _fail(path, "must be a JSON object")
    return value


def _fields(
    value: Mapping[str, Any],
    expected: frozenset[str],
    path: str,
) -> None:
    missing = sorted(expected - set(value))
    unknown = sorted(set(value) - expected)
    if missing:
        _fail(path, f"missing field(s): {', '.join(missing)}")
    if unknown:
        _fail(path, f"unknown field(s): {', '.join(unknown)}")


def _string(value: object, path: str) -> str:
    if not isinstance(value, str) or not value:
        _fail(path, "must be a non-empty string")
    return value


def _sha256(value: object, path: str) -> str:
    if not isinstance(value, str) or _SHA256.fullmatch(value) is None:
        _fail(path, "must be a lowercase SHA-256 digest")
    return value


def _positive(value: object, path: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        _fail(path, "must be a positive integer")
    return value


def _exact(value: object, expected: object, path: str) -> None:
    if value != expected:
        _fail(path, f"must be {expected!r}, got {value!r}")


def _reject_absolute_paths(value: object, path: str = "$") -> None:
    if isinstance(value, str):
        # FastDB error paths are JSON-pointer-like semantic paths, not
        # filesystem locations.
        if path.endswith(".fastdb_path"):
            return
        if (
            PurePosixPath(value).is_absolute()
            or PureWindowsPath(value).is_absolute()
            or re.match(r"^[A-Za-z]:[\\/]", value)
        ):
            _fail(path, f"absolute path is not portable: {value!r}")
    elif isinstance(value, list):
        for index, item in enumerate(value):
            _reject_absolute_paths(item, f"{path}[{index}]")
    elif isinstance(value, dict):
        for key, item in value.items():
            _reject_absolute_paths(item, f"{path}.{key}")


def _validate_release_ref(
    value: object,
    descriptor_sha256: str,
    path: str,
) -> None:
    release = _object(value, path)
    _fields(release, _RELEASE_REF_FIELDS, path)
    _exact(release["schema"], "c-two.contract-release-ref.v1", f"{path}.schema")
    _exact(release["contract_schema"], "c-two.contract.v2", f"{path}.contract_schema")
    _exact(
        release["descriptor_sha256"],
        descriptor_sha256,
        f"{path}.descriptor_sha256",
    )
    crm = _object(release["crm"], f"{path}.crm")
    _fields(crm, _CRM_FIELDS, f"{path}.crm")
    for field in _CRM_FIELDS:
        _string(crm[field], f"{path}.crm.{field}")


def _validate_row(value: object, row_id: str, index: int) -> None:
    path = f"$.rows[{index}]"
    row = _object(value, path)
    _fields(row, _ROW_FIELDS, path)
    _exact(row["id"], row_id, f"{path}.id")
    payload, mode = row_id.split("__")
    _exact(row["payload"], payload, f"{path}.payload")
    _exact(row["mode"], mode, f"{path}.mode")
    _exact(
        row["host_language"],
        _HOST_LANGUAGE[mode],
        f"{path}.host_language",
    )
    _exact(row["observed_path"], _OBSERVED_PATH[mode], f"{path}.observed_path")
    descriptor_sha256 = _sha256(
        row["descriptor_sha256"],
        f"{path}.descriptor_sha256",
    )
    _validate_release_ref(
        row["contract_release_ref"],
        descriptor_sha256,
        f"{path}.contract_release_ref",
    )
    if payload == "no-payload":
        _exact(row["fastdb_spec_sha256"], None, f"{path}.fastdb_spec_sha256")
    else:
        _sha256(row["fastdb_spec_sha256"], f"{path}.fastdb_spec_sha256")
    route = _object(row["route"], f"{path}.route")
    _fields(route, _ROUTE_FIELDS, f"{path}.route")
    _string(route["uid"], f"{path}.route.uid")
    _positive(route["revision"], f"{path}.route.revision")
    _sha256(row["logical_result_sha256"], f"{path}.logical_result_sha256")
    counters = _object(row["path_counters"], f"{path}.path_counters")
    _fields(counters, _COUNTER_FIELDS, f"{path}.path_counters")
    expected_calls = 2 if row_id == OPAQUE_ROW_ID else 1
    _exact(
        counters["requests"],
        expected_calls,
        f"{path}.path_counters.requests",
    )
    _exact(
        counters["responses"],
        expected_calls,
        f"{path}.path_counters.responses",
    )
    lifetime = _object(row["lifetime"], f"{path}.lifetime")
    _fields(lifetime, _LIFETIME_FIELDS, f"{path}.lifetime")
    _exact(lifetime["close_idempotent"], True, f"{path}.lifetime.close_idempotent")
    if payload == "no-payload":
        _exact(
            lifetime["checked_view_invalidated"],
            None,
            f"{path}.lifetime.checked_view_invalidated",
        )
        _exact(
            lifetime["materialized_survived"],
            None,
            f"{path}.lifetime.materialized_survived",
        )
    else:
        _exact(
            lifetime["checked_view_invalidated"],
            True,
            f"{path}.lifetime.checked_view_invalidated",
        )
        _exact(
            lifetime["materialized_survived"],
            True,
            f"{path}.lifetime.materialized_survived",
        )
    _exact(row["status"], "passed", f"{path}.status")


def validate_receipt(
    receipt: object,
    *,
    expected_stage: str,
) -> dict[str, Any]:
    root = _object(receipt, "$")
    _fields(root, _ROOT_FIELDS, "$")
    _reject_absolute_paths(root)
    _exact(root["schema"], RECEIPT_SCHEMA, "$.schema")
    if expected_stage not in ("development", "candidate"):
        raise ValueError(f"unsupported evidence stage: {expected_stage!r}")
    _exact(root["evidence_stage"], expected_stage, "$.evidence_stage")

    runtime = _object(root["runtime"], "$.runtime")
    _fields(runtime, _RUNTIME_FIELDS, "$.runtime")
    _string(runtime["node_version"], "$.runtime.node_version")
    _string(runtime["platform"], "$.runtime.platform")
    _string(runtime["arch"], "$.runtime.arch")
    _exact(runtime["browser_runtime"], "unverified", "$.runtime.browser_runtime")

    packages = _object(root["packages"], "$.packages")
    _fields(packages, _PACKAGE_FIELDS, "$.packages")
    for field in _PACKAGE_FIELDS:
        _sha256(packages[field], f"$.packages.{field}")

    rows = root["rows"]
    if not isinstance(rows, list):
        _fail("$.rows", "must be a JSON array")
    row_ids = [
        row.get("id") if isinstance(row, dict) else None
        for row in rows
    ]
    duplicates = sorted(
        str(row_id)
        for row_id, count in Counter(row_ids).items()
        if row_id is not None and count > 1
    )
    if duplicates:
        _fail("$.rows", f"duplicate row ID(s): {', '.join(duplicates)}")
    missing = sorted(set(EXPECTED_ROW_IDS) - set(row_ids))
    unexpected = sorted(str(item) for item in set(row_ids) - set(EXPECTED_ROW_IDS))
    if missing:
        _fail("$.rows", f"missing row ID(s): {', '.join(missing)}")
    if unexpected:
        _fail("$.rows", f"unexpected row ID(s): {', '.join(unexpected)}")
    if tuple(row_ids) != EXPECTED_ROW_IDS:
        _fail("$.rows", "rows are not in stable EXPECTED_ROW_IDS order")
    for index, row_id in enumerate(EXPECTED_ROW_IDS):
        _validate_row(rows[index], row_id, index)

    negative = _object(root["negative_evidence"], "$.negative_evidence")
    _fields(negative, _NEGATIVE_FIELDS, "$.negative_evidence")
    digest = _object(negative["digest_mismatch"], "$.negative_evidence.digest_mismatch")
    _fields(digest, _DIGEST_FIELDS, "$.negative_evidence.digest_mismatch")
    _exact(digest["verified"], True, "$.negative_evidence.digest_mismatch.verified")
    _exact(
        digest["row_id"],
        DIGEST_ROW_ID,
        "$.negative_evidence.digest_mismatch.row_id",
    )
    cause = _object(digest["fields"], "$.negative_evidence.digest_mismatch.fields")
    _fields(cause, _FASTDB_CAUSE_FIELDS, "$.negative_evidence.digest_mismatch.fields")
    if cause != FROZEN_FASTDB_DIGEST_MISMATCH_CAUSE:
        _fail(
            "$.negative_evidence.digest_mismatch.fields",
            "must equal the frozen FastDB digest mismatch cause",
        )

    opaque = _object(negative["opaque_allocator"], "$.negative_evidence.opaque_allocator")
    _fields(opaque, _OPAQUE_FIELDS, "$.negative_evidence.opaque_allocator")
    _exact(opaque["verified"], True, "$.negative_evidence.opaque_allocator.verified")
    _exact(opaque["released"], 1, "$.negative_evidence.opaque_allocator.released")
    _exact(
        opaque["row_id"],
        OPAQUE_ROW_ID,
        "$.negative_evidence.opaque_allocator.row_id",
    )

    cleanup = _object(root["cleanup"], "$.cleanup")
    _fields(cleanup, _CLEANUP_FIELDS, "$.cleanup")
    for field in _CLEANUP_FIELDS:
        _exact(cleanup[field], True, f"$.cleanup.{field}")
    return dict(root)


def _reject_duplicate_key(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ReceiptValidationError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def load_and_validate_receipt(
    path: Path,
    *,
    expected_stage: str,
) -> dict[str, Any]:
    try:
        value = json.loads(
            path.read_text(encoding="utf-8"),
            object_pairs_hook=_reject_duplicate_key,
            parse_constant=lambda value: _fail("$", f"invalid JSON constant {value}"),
        )
    except ReceiptValidationError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ReceiptValidationError(f"cannot load receipt: {error}") from error
    return validate_receipt(value, expected_stage=expected_stage)


def write_receipt(
    path: Path,
    receipt: object,
    *,
    expected_stage: str,
) -> None:
    validated = validate_receipt(receipt, expected_stage=expected_stage)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(
            validated,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        )
        + "\n",
        encoding="utf-8",
    )
