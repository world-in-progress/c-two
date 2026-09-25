from __future__ import annotations

from collections import Counter
import json
from pathlib import Path, PurePosixPath, PureWindowsPath
import re
from typing import Any, Mapping, NoReturn


PAYLOADS = ("no-payload", "record-v1", "object-graph-v1")
DIRECTIONS = (
    "rust-client__rust-host",
    "rust-client__python-host",
    "python-client__rust-host",
)
TRANSPORTS = ("direct", "relay")
EXPECTED_ROW_IDS = tuple(
    f"{payload}__{direction}__{transport}"
    for payload in PAYLOADS
    for direction in DIRECTIONS
    for transport in TRANSPORTS
)

RECEIPT_SCHEMA = "c-two.portable-matrix-receipt.v1"
RELEASE_REF_SCHEMA = "c-two.contract-release-ref.v1"
CONTRACT_SCHEMA = "c-two.contract.v2"

_ROOT_FIELDS = frozenset(("schema", "evidence_stage", "rows"))
_ROW_FIELDS = frozenset(
    (
        "id",
        "payload",
        "client_language",
        "host_language",
        "transport",
        "observed_path",
        "descriptor_sha256",
        "contract_release_ref",
        "fastdb_spec_sha256",
        "route",
        "logical_result_sha256",
        "packages",
        "c3_sha256",
        "path_counters",
        "status",
    )
)
_RELEASE_REF_FIELDS = frozenset(
    ("schema", "contract_schema", "descriptor_sha256", "crm")
)
_CRM_FIELDS = frozenset(("namespace", "name", "version"))
_ROUTE_FIELDS = frozenset(("uid", "revision"))
_PACKAGE_FIELDS = frozenset(("client_sha256", "host_sha256"))
_PATH_COUNTER_FIELDS = frozenset(("requests", "responses"))
_SHA256 = re.compile(r"^[0-9a-f]{64}$")


class ReceiptValidationError(ValueError):
    """A portable matrix receipt is incomplete, ambiguous, or non-portable."""


def _fail(path: str, message: str) -> NoReturn:
    raise ReceiptValidationError(f"{path}: {message}")


def _require_object(value: object, path: str) -> Mapping[str, Any]:
    if not isinstance(value, dict):
        _fail(path, "must be a JSON object")
    return value


def _reject_unknown_fields(
    value: Mapping[str, Any],
    allowed: frozenset[str],
    path: str,
) -> None:
    unknown = sorted(set(value) - allowed)
    if unknown:
        _fail(path, f"unknown field(s): {', '.join(unknown)}")
    missing = sorted(allowed - set(value))
    if missing:
        _fail(path, f"missing field(s): {', '.join(missing)}")


def _require_string(value: object, path: str) -> str:
    if not isinstance(value, str) or not value:
        _fail(path, "must be a non-empty string")
    return value


def _require_exact(value: object, expected: object, path: str) -> None:
    if value != expected:
        _fail(path, f"must be {expected!r}, got {value!r}")


def _require_sha256(value: object, path: str) -> str:
    if not isinstance(value, str) or _SHA256.fullmatch(value) is None:
        _fail(path, "must be a lowercase 64-character SHA-256 digest")
    return value


def _require_positive_integer(value: object, path: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        _fail(path, "must be a positive integer")
    return value


def _looks_absolute_path(value: str) -> bool:
    return (
        PurePosixPath(value).is_absolute()
        or PureWindowsPath(value).is_absolute()
        or bool(re.match(r"^[A-Za-z]:[\\/]", value))
    )


def _reject_absolute_paths(value: object, path: str = "$") -> None:
    if isinstance(value, str):
        if _looks_absolute_path(value):
            _fail(path, f"absolute path is not portable: {value!r}")
        return
    if isinstance(value, list):
        for index, item in enumerate(value):
            _reject_absolute_paths(item, f"{path}[{index}]")
        return
    if isinstance(value, dict):
        for key, item in value.items():
            _reject_absolute_paths(item, f"{path}.{key}")


def _validate_release_ref(
    value: object,
    *,
    descriptor_sha256: str,
    path: str,
) -> None:
    release_ref = _require_object(value, path)
    _reject_unknown_fields(release_ref, _RELEASE_REF_FIELDS, path)
    _require_exact(release_ref["schema"], RELEASE_REF_SCHEMA, f"{path}.schema")
    _require_exact(
        release_ref["contract_schema"],
        CONTRACT_SCHEMA,
        f"{path}.contract_schema",
    )
    _require_exact(
        release_ref["descriptor_sha256"],
        descriptor_sha256,
        f"{path}.descriptor_sha256",
    )
    crm_path = f"{path}.crm"
    crm = _require_object(release_ref["crm"], crm_path)
    _reject_unknown_fields(crm, _CRM_FIELDS, crm_path)
    for field in ("namespace", "name", "version"):
        _require_string(crm[field], f"{crm_path}.{field}")


def _validate_row(value: object, expected_id: str, index: int) -> None:
    path = f"$.rows[{index}]"
    row = _require_object(value, path)
    _reject_unknown_fields(row, _ROW_FIELDS, path)
    _require_exact(row["id"], expected_id, f"{path}.id")

    payload, client_role, host_role, transport = expected_id.split("__")
    client_language = client_role.removesuffix("-client")
    host_language = host_role.removesuffix("-host")
    _require_exact(row["payload"], payload, f"{path}.payload")
    _require_exact(
        row["client_language"],
        client_language,
        f"{path}.client_language",
    )
    _require_exact(
        row["host_language"],
        host_language,
        f"{path}.host_language",
    )
    _require_exact(row["transport"], transport, f"{path}.transport")

    descriptor_sha256 = _require_sha256(
        row["descriptor_sha256"],
        f"{path}.descriptor_sha256",
    )
    _validate_release_ref(
        row["contract_release_ref"],
        descriptor_sha256=descriptor_sha256,
        path=f"{path}.contract_release_ref",
    )

    fastdb_digest = row["fastdb_spec_sha256"]
    if payload == "no-payload":
        if fastdb_digest is not None:
            _fail(
                f"{path}.fastdb_spec_sha256",
                "no-payload row must use explicit null",
            )
    elif fastdb_digest is None:
        _fail(
            f"{path}.fastdb_spec_sha256",
            f"{payload} row must contain a FastDB spec SHA-256",
        )
    else:
        _require_sha256(fastdb_digest, f"{path}.fastdb_spec_sha256")

    route_path = f"{path}.route"
    route = _require_object(row["route"], route_path)
    _reject_unknown_fields(route, _ROUTE_FIELDS, route_path)
    _require_string(route["uid"], f"{route_path}.uid")
    _require_positive_integer(route["revision"], f"{route_path}.revision")

    _require_sha256(
        row["logical_result_sha256"],
        f"{path}.logical_result_sha256",
    )

    packages_path = f"{path}.packages"
    packages = _require_object(row["packages"], packages_path)
    _reject_unknown_fields(packages, _PACKAGE_FIELDS, packages_path)
    for field in ("client_sha256", "host_sha256"):
        _require_sha256(packages[field], f"{packages_path}.{field}")

    counters_path = f"{path}.path_counters"
    counters = _require_object(row["path_counters"], counters_path)
    _reject_unknown_fields(counters, _PATH_COUNTER_FIELDS, counters_path)
    requests = _require_positive_integer(
        counters["requests"],
        f"{counters_path}.requests",
    )
    responses = _require_positive_integer(
        counters["responses"],
        f"{counters_path}.responses",
    )

    if transport == "relay":
        if row["observed_path"] != "ExplicitRelay":
            _fail(
                f"{path}.observed_path",
                "relay row must observe ExplicitRelay",
            )
        _require_sha256(row["c3_sha256"], f"{path}.c3_sha256")
        if requests <= 0 or responses <= 0:
            _fail(path, "relay request/response path counters must be positive")
    else:
        if row["observed_path"] != "DirectIpc":
            _fail(
                f"{path}.observed_path",
                "direct row must observe DirectIpc",
            )
        if row["c3_sha256"] is not None:
            _fail(f"{path}.c3_sha256", "direct row must use explicit null")

    if row["status"] != "passed":
        _fail(f"{path}.status", "status must be 'passed'")


def validate_receipt(
    receipt: object,
    *,
    expected_stage: str,
) -> dict[str, Any]:
    """Validate one complete, strictly ordered 18-row matrix receipt."""
    root = _require_object(receipt, "$")
    _reject_unknown_fields(root, _ROOT_FIELDS, "$")
    _reject_absolute_paths(root)
    _require_exact(root["schema"], RECEIPT_SCHEMA, "$.schema")
    if expected_stage not in ("development", "candidate"):
        raise ValueError(f"unsupported expected evidence stage: {expected_stage!r}")
    _require_exact(root["evidence_stage"], expected_stage, "$.evidence_stage")

    rows = root["rows"]
    if not isinstance(rows, list):
        _fail("$.rows", "must be a JSON array")
    row_ids: list[object] = [
        row.get("id") if isinstance(row, dict) else None
        for row in rows
    ]
    duplicate_ids = sorted(
        str(row_id)
        for row_id, count in Counter(row_ids).items()
        if row_id is not None and count > 1
    )
    if duplicate_ids:
        _fail("$.rows", f"duplicate row ID(s): {', '.join(duplicate_ids)}")

    expected_set = set(EXPECTED_ROW_IDS)
    actual_set = set(row_ids)
    unexpected = sorted(str(row_id) for row_id in actual_set - expected_set)
    if unexpected:
        _fail("$.rows", f"unexpected row ID(s): {', '.join(unexpected)}")
    missing = sorted(expected_set - actual_set)
    if missing:
        _fail("$.rows", f"missing row ID(s): {', '.join(missing)}")
    if tuple(row_ids) != EXPECTED_ROW_IDS:
        _fail("$.rows", "rows are not in stable EXPECTED_ROW_IDS order")

    for index, (row, expected_id) in enumerate(zip(rows, EXPECTED_ROW_IDS)):
        _validate_row(row, expected_id, index)
    return dict(root)


def _duplicate_key_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ReceiptValidationError(f"duplicate JSON key: {key!r}")
        result[key] = value
    return result


def _reject_non_finite(value: str) -> NoReturn:
    raise ReceiptValidationError(f"non-finite JSON number is forbidden: {value}")


def load_and_validate_receipt(
    path: Path,
    *,
    expected_stage: str,
) -> dict[str, Any]:
    """Load strict JSON from ``path`` and validate the complete receipt."""
    try:
        receipt = json.loads(
            path.read_text(encoding="utf-8"),
            object_pairs_hook=_duplicate_key_object,
            parse_constant=_reject_non_finite,
        )
    except ReceiptValidationError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ReceiptValidationError(f"{path}: invalid receipt JSON: {error}") from error
    return validate_receipt(receipt, expected_stage=expected_stage)


def write_receipt(
    path: Path,
    receipt: object,
    *,
    expected_stage: str,
) -> None:
    """Validate and write deterministic UTF-8 JSON with one trailing newline."""
    validated = validate_receipt(receipt, expected_stage=expected_stage)
    encoded = (
        json.dumps(
            validated,
            ensure_ascii=False,
            allow_nan=False,
            sort_keys=True,
            separators=(",", ":"),
        )
        + "\n"
    ).encode("utf-8")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(encoded)
