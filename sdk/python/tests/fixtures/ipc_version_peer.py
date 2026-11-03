"""One native fixed-record peer, shared by Python and official Node clients."""
from contextlib import contextmanager
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import uuid

import pytest
import c_two as cc
import c_two.endpoint
from tests.fixtures.process_control import readline_with_timeout

REPOSITORY = Path(__file__).resolve().parents[4]


@pytest.fixture(scope="session")
def ipc_version_peer_binary(tmp_path_factory):
    work = tmp_path_factory.mktemp("ipc-version-build")
    source = Path(__file__).with_suffix(".rs")
    shutil.copyfile(source, work / "main.rs")
    (work / "Cargo.toml").write_text(
        '[package]\nname="c2-ipc-version-peer"\nversion="0.0.0"\nedition="2024"\n'
        '[[bin]]\nname="c2-ipc-version-peer"\npath="main.rs"\n[dependencies]\n'
        f'c2-local={{path={json.dumps(str(REPOSITORY / "core/transport/c2-local"))}}}\n'
        'tokio={version="1",features=["rt","macros","io-util","io-std","time"]}\n',
        encoding="utf-8",
    )
    target = Path(os.environ.get("CARGO_TARGET_DIR", REPOSITORY / "core/target"))
    subprocess.run(
        ["cargo", "build", "--quiet", "--manifest-path", str(work / "Cargo.toml")],
        cwd=work, env={**os.environ, "CARGO_TARGET_DIR": str(target)},
        check=True, capture_output=True, text=True, timeout=180,
    )
    return target / "debug" / ("c2-ipc-version-peer.exe" if os.name == "nt" else "c2-ipc-version-peer")


@contextmanager
def running_version_peer(binary, reply):
    # macOS tempdir paths may exceed sun_path; this is a private final root.
    root = tempfile.mkdtemp(prefix="c2v-", dir=None if os.name == "nt" else "/tmp")
    context = cc.endpoint.local_endpoint_context(**({"root": root} if os.name != "nt" else {}))
    address = f"ipc://version-{uuid.uuid4().hex}"
    command = [str(binary), address, root, reply]
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        assert readline_with_timeout(process.stdout, 5).strip() == "READY"
        yield address, context
    finally:
        stdout, stderr = process.communicate(input="done\n", timeout=12)
        if os.name != "nt":
            assert not Path(context.endpoint_name(address)).exists(), root
            assert not list(Path(root).glob("*.lease")), root
        else:
            # Rebinding after exit checks Windows' Global listener lease release.
            subprocess.run([*command[:-1], "probe"], check=True, capture_output=True, text=True, timeout=4)
        assert process.returncode == 0, (root, stdout, stderr)
        assert stdout.strip() == "COUNTS 1 0", (root, stdout, stderr)
        shutil.rmtree(root)  # Retain the private directory on any cleanup/count failure.
