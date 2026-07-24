from __future__ import annotations

from copy import deepcopy
import json
from pathlib import Path
import subprocess
import sys

import pytest


REPOSITORY = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPOSITORY))

from tools.local_rc.package_consumers import (  # noqa: E402
    RECEIPT_SCHEMA,
    ConsumerError,
    clean_consumer_environment,
    load_and_validate_receipt,
    resolve_python_interpreter,
    validate_installed_origin,
    validate_receipt,
    write_receipt,
)
from tools.local_rc.process_guard import guarded_process  # noqa: E402


def _digest(value: str) -> str:
    return value * 64


def _receipt() -> dict[str, object]:
    return {
        "schema": RECEIPT_SCHEMA,
        "evidence_stage": "candidate",
        "candidate_manifest_sha256": _digest("a"),
        "consumers": {
            "rust": {
                "package_sha256": {
                    "c-two": _digest("b"),
                    "fastdb": _digest("c"),
                },
                "version_only_manifest": True,
                "offline_local_registry": True,
                "real_call": True,
                "status": "passed",
            },
            "python-current": {
                "package_sha256": {
                    "c-two": _digest("d"),
                    "fastdb4py": _digest("e"),
                    "numpy": _digest("5"),
                },
                "no_index": True,
                "installed_only": True,
                "real_call": True,
                "lifetime": True,
                "status": "passed",
            },
            "python-3.10": {
                "package_sha256": {
                    "c-two": _digest("f"),
                    "fastdb4py": _digest("1"),
                    "numpy": _digest("6"),
                },
                "no_index": True,
                "installed_only": True,
                "real_call": True,
                "lifetime": True,
                "status": "passed",
            },
            "node": {
                "package_sha256": {
                    "@c-two/c2-mem-ffi": _digest("2"),
                    "fastdb4ts": _digest("3"),
                    "typescript": _digest("4"),
                },
                "tarballs_only": True,
                "sibling_aliases": False,
                "real_call": True,
                "status": "passed",
            },
        },
        "cleanup": {
            "host_processes_stopped": True,
            "relay_processes_stopped": True,
            "temporary_environments_removed": True,
        },
    }


def test_package_consumer_receipt_is_canonical(tmp_path: Path) -> None:
    receipt = _receipt()
    destination = tmp_path / "package-consumer-receipt.v1.json"

    write_receipt(destination, receipt)
    first = destination.read_bytes()
    write_receipt(destination, receipt)

    assert destination.read_bytes() == first
    assert first.endswith(b"\n")
    assert load_and_validate_receipt(destination) == receipt


@pytest.mark.parametrize(
    ("mutate", "message"),
    [
        (
            lambda receipt: receipt.__setitem__(
                "candidate_manifest_sha256",
                "invalid",
            ),
            "SHA-256",
        ),
        (
            lambda receipt: receipt["consumers"]["rust"].__setitem__(
                "version_only_manifest",
                False,
            ),
            "version_only_manifest",
        ),
        (
            lambda receipt: receipt["consumers"]["python-3.10"].__setitem__(
                "installed_only",
                False,
            ),
            "installed_only",
        ),
        (
            lambda receipt: receipt["consumers"]["node"].__setitem__(
                "sibling_aliases",
                True,
            ),
            "sibling_aliases",
        ),
        (
            lambda receipt: receipt["cleanup"].__setitem__(
                "relay_processes_stopped",
                False,
            ),
            "relay_processes_stopped",
        ),
        (
            lambda receipt: receipt["consumers"]["rust"].__setitem__(
                "unexpected",
                True,
            ),
            "unknown field",
        ),
    ],
)
def test_incomplete_or_overclaimed_consumers_are_rejected(
    mutate,
    message: str,
) -> None:
    receipt = _receipt()
    mutate(receipt)

    with pytest.raises(ConsumerError, match=message):
        validate_receipt(receipt)


def test_duplicate_json_keys_are_rejected(tmp_path: Path) -> None:
    destination = tmp_path / "receipt.json"
    destination.write_text(
        json.dumps(_receipt()).replace(
            '"evidence_stage": "candidate"',
            '"evidence_stage": "candidate", "evidence_stage": "candidate"',
            1,
        ),
        encoding="utf-8",
    )

    with pytest.raises(ConsumerError, match="duplicate JSON key"):
        load_and_validate_receipt(destination)


def test_clean_environment_removes_checkout_resolution_state() -> None:
    environment = clean_consumer_environment(
        {
            "PATH": "/usr/bin",
            "HOME": "/tmp/home",
            "PYTHONPATH": str(REPOSITORY / "sdk/python/src"),
            "VIRTUAL_ENV": str(REPOSITORY / ".venv"),
            "UV_PROJECT_ENVIRONMENT": str(REPOSITORY / ".venv"),
            "PIP_EDITABLE": str(REPOSITORY),
            "CARGO_TARGET_DIR": str(REPOSITORY / "core/target"),
        }
    )

    assert environment["PATH"] == "/usr/bin"
    assert environment["HOME"] == "/tmp/home"
    assert "PYTHONPATH" not in environment
    assert "VIRTUAL_ENV" not in environment
    assert "UV_PROJECT_ENVIRONMENT" not in environment
    assert "PIP_EDITABLE" not in environment
    assert "CARGO_TARGET_DIR" not in environment


def test_python_interpreter_resolution_has_no_machine_specific_fallback(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    interpreter = tmp_path / "python3.10"
    interpreter.write_text("", encoding="utf-8")
    monkeypatch.setenv("C2_LRC_PYTHON_310", str(interpreter))
    monkeypatch.setattr("shutil.which", lambda _: None)

    assert (
        resolve_python_interpreter(
            "C2_LRC_PYTHON_310",
            "python3.10",
        )
        == interpreter
    )

    monkeypatch.delenv("C2_LRC_PYTHON_310")
    with pytest.raises(ConsumerError, match="set C2_LRC_PYTHON_310"):
        resolve_python_interpreter(
            "C2_LRC_PYTHON_310",
            "python3.10",
        )


def test_installed_origins_must_live_under_the_consumer_root(
    tmp_path: Path,
) -> None:
    environment = tmp_path / "venv"
    installed = environment / "lib/python/site-packages/c_two/__init__.py"
    installed.parent.mkdir(parents=True)
    installed.write_text("", encoding="utf-8")

    validate_installed_origin(
        installed,
        environment,
        forbidden_roots=(REPOSITORY, REPOSITORY.parent / "fastdb"),
    )

    with pytest.raises(ConsumerError, match="consumer root"):
        validate_installed_origin(
            REPOSITORY / "sdk/python/src/c_two/__init__.py",
            environment,
            forbidden_roots=(REPOSITORY, REPOSITORY.parent / "fastdb"),
        )


def test_receipt_does_not_accept_reordered_or_missing_consumer(
    tmp_path: Path,
) -> None:
    receipt = deepcopy(_receipt())
    receipt["consumers"].pop("node")
    with pytest.raises(ConsumerError, match="missing field"):
        validate_receipt(receipt)


def test_process_guard_stops_the_entire_child_process(tmp_path: Path) -> None:
    with guarded_process(
        [
            sys.executable,
            "-c",
            "import time; time.sleep(60)",
        ],
        cwd=tmp_path,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    ) as process:
        assert process.poll() is None

    assert process.poll() is not None
