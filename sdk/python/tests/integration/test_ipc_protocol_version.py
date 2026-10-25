"""Public Python errors from real UDS/Named Pipe version negotiation."""
import copy
import pickle

import pytest
import c_two as cc
from c_two.config.settings import settings
from c_two.error import CoreError, ProtocolViolation
from c_two.transport.registry import _ProcessRegistry
from tests.fixtures.ipc_version_peer import ipc_version_peer_binary, running_version_peer

pytestmark = pytest.mark.timeout(300)


@cc.crm(namespace="test.ipc_version", version="1.0.0")
class VersionContract:
    def ping(self) -> None: ...


@pytest.fixture(autouse=True)
def _clean_runtime():
    previous = settings._relay_anchor_address
    _ProcessRegistry.reset()
    settings.relay_anchor_address = None
    try:
        yield
    finally:
        _ProcessRegistry.reset()
        settings._relay_anchor_address = previous


@pytest.mark.parametrize(("reply", "peer_version"), [
    ("mismatch15", 15), ("mismatch13", 13), ("mismatch-max", 0xFFFFFFFF),
    ("bad-magic", None), ("accepted15", None), ("mismatch14", None),
])
def test_connect_fixed_version_peer(ipc_version_peer_binary, reply, peer_version):
    with running_version_peer(ipc_version_peer_binary, reply) as (address, context):
        cc.set_local_endpoint(root=context.root)
        with pytest.raises(CoreError if peer_version is not None else ProtocolViolation) as failure:
            cc.connect(VersionContract, name="version", address=address, timeout=3)
        # Malformed handshakes retain the public semantic ProtocolViolation.
        # Only a complete, consistent refusal projects the new local category.
        error = failure.value if peer_version is not None else failure.value.__cause__
        assert type(error) is CoreError
        assert (error.io_operation, error.io_kind, error.raw_os_error) == (None, None, None)
        if peer_version is None:
            assert error.category != "protocol_version_mismatch"
            assert (error.local_protocol_version, error.peer_protocol_version) == (None, None)
        else:
            assert error.category == "protocol_version_mismatch"
            assert (error.local_protocol_version, error.peer_protocol_version) == (14, peer_version)
            assert type(error.local_protocol_version) is type(error.peer_protocol_version) is int
            assert (error.transport_kind, error.transport_phase) == ("ipc", "pre_dispatch")
            assert error.fallback_eligible is False
            assert (error.code, error.name, error.error_bytes) == (None, None, None)
        for restored in (copy.copy(error), pickle.loads(pickle.dumps(error))):
            assert type(restored) is CoreError
            assert restored is not error
            assert restored.args == error.args
            assert restored.__dict__ == error.__dict__
