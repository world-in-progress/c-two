"""Read real internal registration identity for relay control tests."""
from __future__ import annotations

import os
from pathlib import Path
import subprocess

import pytest

import c_two.endpoint


REPOSITORY = Path(__file__).resolve().parents[4]


@pytest.fixture(scope="session")
def route_scope_attestor_binary() -> Path:
    selected = os.environ.get("CTWO_TEST_ROUTE_SCOPE_ATTESTOR_BIN")
    target = Path(os.environ.get("CARGO_TARGET_DIR", REPOSITORY / "core/target"))
    name = "c2-route-scope-attestor.exe" if os.name == "nt" else "c2-route-scope-attestor"
    binary = Path(selected) if selected else target / "debug" / name
    if not binary.is_file():
        pytest.fail(
            f"Route scope attestor is not built: {binary}. From the checkout root, "
            "run `python tools/dev/build_route_scope_attestor.py` with the same "
            "CARGO_TARGET_DIR, then pass its path via CTWO_TEST_ROUTE_SCOPE_ATTESTOR_BIN "
            "if selecting a binary explicitly.",
            pytrace=False,
        )
    return binary.resolve()


def attested_registration_scope(binary: Path, name: str, address: str,
                                server_id: str, server_instance_id: str) -> dict[str, str]:
    context = c_two.endpoint.local_endpoint_context()
    result = subprocess.run(
        [str(binary), address, name, context.root or "-"],
        check=True, capture_output=True, text=True, timeout=15,
    )
    route_uid = result.stdout.strip()
    assert route_uid, result.stderr
    return {
        "name": name, "server_id": server_id,
        "server_instance_id": server_instance_id, "route_uid": route_uid,
    }
