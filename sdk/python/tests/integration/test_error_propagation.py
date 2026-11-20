"""
Comprehensive error propagation tests for the c-two RPC framework.

Verifies that every error type is correctly classified, propagated as a single
layer (no re-wrapping), and projected to proper subclass types.
"""
import time
import threading

import pytest

from tests.fixtures import local_server

import c_two as cc
from c_two.transport.client.util import ping
from c_two.error import (
    ErrorCode, CCBaseError, CCError, CoreError,
    ResourceExecutionError, SerializationError, ResourceClosed, ResourceRemoved,
)
from tests.fixtures.hello import HelloImpl
from tests.fixtures.ihello import Hello, HelloData

pytestmark = pytest.mark.timeout(30)

# ---------------------------------------------------------------------------
# Unique address factory
# ---------------------------------------------------------------------------
_counter = 0
_lock = threading.Lock()

def _next_id():
    global _counter
    with _lock:
        _counter += 1
        return _counter


# ---------------------------------------------------------------------------
# ErrorHello — CRM that deliberately raises in every overridden method
# ---------------------------------------------------------------------------
class ErrorHello(Hello):
    def add(self, a: int, b: int) -> int:
        raise ValueError('division by zero')

    def greeting(self, name: str) -> str:
        raise TypeError('name must be str')

    def get_data(self, id: int) -> HelloData:
        raise RuntimeError('database unavailable')


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------
@pytest.fixture
def error_server():
    """Start a server backed by ErrorHello, yield its address, then shut down."""
    address = f'ipc://error_test_{_next_id()}'
    server = local_server(address)
    server.register_crm(Hello, ErrorHello(), name='hello')
    server.start()
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        try:
            if ping(address, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.05)
    yield address
    server.shutdown()


# ---------------------------------------------------------------------------
# Class 1 — CRM execution errors
# ---------------------------------------------------------------------------
class TestResourceExecutionError:
    """Plain exceptions raised inside a CRM method surface as ResourceExecutionError."""

    def test_value_error_in_crm(self, error_server):
        with cc.connect(Hello, name='hello', address=error_server) as crm:
            with pytest.raises(ResourceExecutionError) as exc_info:
                crm.add(1, 2)
            err = exc_info.value
            assert err.code == ErrorCode.ResourceFunctionExecuting
            assert 'division by zero' in str(err)
            assert err.transport_phase is None

    def test_type_error_in_crm(self, error_server):
        with cc.connect(Hello, name='hello', address=error_server) as crm:
            with pytest.raises(ResourceExecutionError) as exc_info:
                crm.greeting('test')
            assert 'name must be str' in str(exc_info.value)

    def test_runtime_error_in_crm(self, error_server):
        with cc.connect(Hello, name='hello', address=error_server) as crm:
            with pytest.raises(ResourceExecutionError) as exc_info:
                crm.get_data(1)
            assert 'database unavailable' in str(exc_info.value)

    def test_error_is_proper_subclass(self, error_server):
        with cc.connect(Hello, name='hello', address=error_server) as crm:
            with pytest.raises(ResourceExecutionError) as exc_info:
                crm.add(1, 2)
            err = exc_info.value
            assert isinstance(err, CCError)
            assert isinstance(err, CCBaseError)
            assert isinstance(err, Exception)
            assert err.side == 'resource'
            assert err.stage == 'execute'

    def test_no_multi_layer_wrapping(self, error_server):
        """Server-side CRM errors must NOT be re-wrapped into a client error."""
        with cc.connect(Hello, name='hello', address=error_server) as crm:
            with pytest.raises(ResourceExecutionError) as exc_info:
                crm.add(1, 2)
            err = exc_info.value
            assert int(err.code) == 3
            assert err.code != ErrorCode.ClientOutputDeserializing

    def test_error_propagates_through_connect_crm(self, error_server):
        """connect_crm must NOT swallow exceptions — they must reach the caller."""
        with pytest.raises(CCBaseError):
            with cc.connect(Hello, name='hello', address=error_server) as crm:
                crm.add(1, 2)


# ---------------------------------------------------------------------------
# Class 2 — Business CCError preservation across transports
# ---------------------------------------------------------------------------
class BusinessHello(Hello):
    def add(self, a: int, b: int) -> int:
        raise ResourceRemoved('grid withdrawn by owner', details={
            'route': 'grid', 'reason': 'owner_withdrawal',
        })

    def greeting(self, name: str) -> str:
        raise ResourceRemoved('grid withdrawn by owner', details={
            'route': 'grid', 'reason': 'owner_withdrawal',
        })

    def get_data(self, id: int) -> HelloData:
        raise ResourceRemoved('grid withdrawn by owner', details={
            'route': 'grid', 'reason': 'owner_withdrawal',
        })


@pytest.fixture
def business_server():
    address = f'ipc://business_test_{_next_id()}'
    server = local_server(address)
    server.register_crm(Hello, BusinessHello(), name='hello')
    server.start()
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        try:
            if ping(address, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.05)
    yield address
    server.shutdown()


class TestBusinessErrorPreservation:
    def test_named_ccerror_keeps_type_and_details_over_ipc(self, business_server):
        with cc.connect(Hello, name='hello', address=business_server) as crm:
            with pytest.raises(ResourceRemoved) as exc_info:
                crm.add(1, 2)
            err = exc_info.value
            assert int(err.code) == 708
            assert err.message == 'grid withdrawn by owner'
            assert err.details == {'route': 'grid', 'reason': 'owner_withdrawal'}
            assert err.transport_phase is None

    def test_named_ccerror_keeps_type_through_same_process_call(self):
        from c_two.transport.client.proxy import CRMProxy

        crm = Hello()
        crm.client = CRMProxy.thread_local(BusinessHello(), name='hello')
        with pytest.raises(ResourceRemoved) as exc_info:
            crm.add(1, 2)
        err = exc_info.value
        assert err.message == 'grid withdrawn by owner'
        assert err.details['reason'] == 'owner_withdrawal'
        assert err.transport_phase is None
        # The business-created original exception object is never mutated.
        original = exc_info.value.__cause__
        assert original is not None
        assert original.transport_phase is None
        assert original.details == {'route': 'grid', 'reason': 'owner_withdrawal'}


# ---------------------------------------------------------------------------
# Class 3 — Client errors
# ---------------------------------------------------------------------------
class TestClientError:
    """Errors triggered by client connectivity problems."""

    def test_connect_to_nonexistent_server(self):
        address = f'ipc://nonexistent_{_next_id()}'
        with pytest.raises(Exception):
            with cc.connect(Hello, name='hello', address=address) as crm:
                crm.greeting('test')

    def test_ping_nonexistent_returns_false(self):
        address = f'ipc://nonexistent_{_next_id()}'
        assert ping(address, timeout=0.5) is False


# ---------------------------------------------------------------------------
# Class 4 — Error round-trips (copy/pickle)
# ---------------------------------------------------------------------------
ROUND_TRIP_CASES = [
    CCError(ErrorCode.Unknown, 'generic failure'),
    CCError(ErrorCode.ClientCallingResource, 'call failed', {'route': 'grid'}),
    ResourceExecutionError('resource raised'),
    SerializationError(ErrorCode.ResourceInputDeserializing, 'bad input'),
    SerializationError(ErrorCode.ClientOutputFromBuffer, 'bad view', {'cause_owner': 'fastdb'}),
    ResourceClosed('route closed', {'route': 'grid'}),
    ResourceRemoved('withdrawn'),
]


class TestErrorRoundTrips:
    @pytest.mark.parametrize('original', ROUND_TRIP_CASES, ids=lambda c: type(c).__name__)
    def test_copy_and_pickle_preserve_type_and_payload(self, original):
        import copy
        import pickle

        for restored in (copy.copy(original), pickle.loads(pickle.dumps(original))):
            assert type(restored) is type(original)
            assert restored.code == original.code
            assert restored.message == original.message
            assert restored.details == original.details
            assert restored.transport_phase == original.transport_phase


# ---------------------------------------------------------------------------
# Class 5 — Single-layer propagation (end-to-end)
# ---------------------------------------------------------------------------
class TestSingleLayerPropagation:
    """End-to-end tests ensuring errors propagate as a single layer."""

    def test_server_error_single_layer(self, error_server):
        """The error string is one line: code name plus the original message."""
        with cc.connect(Hello, name='hello', address=error_server) as crm:
            with pytest.raises(ResourceExecutionError) as exc_info:
                crm.add(1, 2)
            assert str(exc_info.value) == 'ResourceFunctionExecuting: division by zero'

    def test_error_code_correct(self, error_server):
        """Error code must be ResourceFunctionExecuting, not a codec code."""
        with cc.connect(Hello, name='hello', address=error_server) as crm:
            with pytest.raises(ResourceExecutionError) as exc_info:
                crm.add(1, 2)
            assert exc_info.value.code == ErrorCode.ResourceFunctionExecuting
            assert exc_info.value.code != ErrorCode.ResourceOutputSerializing

    def test_original_error_info_preserved(self, error_server):
        """The original exception message survives without being prefixed."""
        with cc.connect(Hello, name='hello', address=error_server) as crm:
            with pytest.raises(ResourceExecutionError) as exc_info:
                crm.greeting('test')
            assert exc_info.value.message == 'name must be str'
