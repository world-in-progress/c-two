"""Read real internal registration identity for relay control tests."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess

import pytest

import c_two.endpoint


REPOSITORY = Path(__file__).resolve().parents[4]


@pytest.fixture(scope="session")
def route_scope_attestor_binary(tmp_path_factory):
    work = tmp_path_factory.mktemp("route-scope-attestor")
    shutil.copyfile(Path(__file__).with_suffix(".rs"), work / "main.rs")
    (work / "Cargo.toml").write_text(
        '[package]\nname="c2-route-scope-attestor"\nversion="0.0.0"\nedition="2024"\n'
        '[[bin]]\nname="c2-route-scope-attestor"\npath="main.rs"\n[dependencies]\n'
        f'c2-config={{path={json.dumps(str(REPOSITORY / "core/foundation/c2-config"))}}}\n'
        f'c2-ipc={{path={json.dumps(str(REPOSITORY / "core/transport/c2-ipc"))}}}\n'
        'tokio={version="1",features=["rt","macros"]}\n',
        encoding="utf-8",
    )
    target = Path(os.environ.get("CARGO_TARGET_DIR", REPOSITORY / "core/target"))
    subprocess.run(
        ["cargo", "build", "--quiet", "--manifest-path", str(work / "Cargo.toml")],
        cwd=work, env={**os.environ, "CARGO_TARGET_DIR": str(target)},
        check=True, capture_output=True, text=True, timeout=180,
    )
    return target / "debug" / ("c2-route-scope-attestor.exe" if os.name == "nt" else "c2-route-scope-attestor")


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
