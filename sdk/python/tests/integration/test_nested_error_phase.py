"""A Rust resource's nested refusal must not describe the outer call's phase."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

import pytest

import c_two as cc
import c_two.contract
import c_two.endpoint
from c_two.config.settings import settings
from c_two.error import ResourceRemoved
from c_two.transport.registry import _reset_default_runtime
from tests.fixtures.process_control import readline_with_timeout


REPOSITORY = Path(__file__).resolve().parents[4]
DESCRIPTOR = REPOSITORY / 'tests/fixtures/contracts/portable-no-payload.contract.json'
RUST_FIXTURE = REPOSITORY / 'sdk/rust/tests/fixtures/nested_error_phase.rs'
RUST_MAIN = REPOSITORY / 'sdk/python/tests/fixtures/nested_error_phase_rust.rs'
TARGET_DIR = REPOSITORY / 'core/target'

pytestmark = pytest.mark.timeout(300)


@cc.crm(namespace='test.portable-matrix.no-payload', version='0.1.0')
class PortableNoPayload:
    @cc.read
    def ping(self) -> None: ...


@pytest.fixture(scope='session')
def nested_error_rust_host_binary(tmp_path_factory):
    work = tmp_path_factory.mktemp('nested-error-host-build')
    source = work / 'src'
    source.mkdir()
    shutil.copyfile(RUST_MAIN, source / 'main.rs')
    shutil.copyfile(RUST_FIXTURE, source / 'nested_error_phase.rs')
    (work / 'Cargo.toml').write_text(
        '[package]\nname="c2-nested-error-host"\nversion="0.0.0"\nedition="2024"\n'
        '[dependencies]\n'
        f'c-two={{path={json.dumps(str(REPOSITORY / "sdk/rust"))}}}\n',
        encoding='utf-8',
    )
    completed = subprocess.run(
        ['cargo', 'build', '--quiet', '--manifest-path', str(work / 'Cargo.toml')],
        cwd=work,
        env={**os.environ, 'CARGO_TARGET_DIR': str(TARGET_DIR)},
        check=False,
        capture_output=True,
        text=True,
        timeout=180,
    )
    assert completed.returncode == 0, completed.stderr
    name = 'c2-nested-error-host.exe' if os.name == 'nt' else 'c2-nested-error-host'
    return TARGET_DIR / 'debug' / name


def test_python_consumer_does_not_inherit_nested_pre_dispatch(
    nested_error_rust_host_binary,
):
    assert json.loads(cc.contract.export_descriptor(PortableNoPayload)) == json.loads(
        DESCRIPTOR.read_text(encoding='utf-8')
    )
    previous_relay = settings._relay_anchor_address  # noqa: SLF001
    _reset_default_runtime()
    settings.relay_anchor_address = None
    counts = None
    try:
        with tempfile.TemporaryDirectory(prefix='c2-ne-', dir=None if os.name == 'nt' else '/tmp') as root:
            if os.name != 'nt':
                cc.endpoint.set_local_endpoint(root=root)
            process = subprocess.Popen(
                [str(nested_error_rust_host_binary), str(DESCRIPTOR), root],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                bufsize=1,
            )
            try:
                assert process.stdout is not None
                ready = readline_with_timeout(process.stdout, 20).split()
                assert len(ready) == 4 and ready[0] == 'READY', ready
                _, address, route_a, route_b = ready
                client = cc.connect(PortableNoPayload, name=route_a, address=address)
                try:
                    with pytest.raises(ResourceRemoved) as failure:
                        client.ping()
                    error = failure.value
                    assert error.details['route'] == route_b
                    assert 'route_uid' not in error.details
                    assert 'transport_phase' not in error.details
                    assert 'dispatch_phase' not in error.details
                    assert 'fallback_eligible' not in error.details
                    cause = error.__cause__
                    assert cause is not None
                    assert cause.transport_phase is None
                    assert cause.fallback_eligible is None
                finally:
                    cc.close(client)
            finally:
                try:
                    stdout, stderr = process.communicate(input='\n', timeout=20)
                except subprocess.TimeoutExpired:
                    process.kill()
                    stdout, stderr = process.communicate()
                    raise AssertionError(f'Rust host did not stop: {stdout}\n{stderr}')
                assert process.returncode == 0, (stdout, stderr)
                lines = stdout.splitlines()
                assert len(lines) == 1 and lines[0].startswith('COUNTS '), (stdout, stderr)
                counts = tuple(map(int, lines[0].split()[1:]))
    finally:
        _reset_default_runtime()
        settings._relay_anchor_address = previous_relay  # noqa: SLF001
    assert counts == (1, 0), 'A executed once and B never entered its callback'
