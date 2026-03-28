"""
Comprehensive error propagation tests for the c-two RPC framework.

Verifies that every error type is correctly classified, propagated as a single
layer (no re-wrapping), and deserialized to proper subclass types.
"""
import time
import threading

import pytest

import c_two as cc
from c_two.transport.server.core import ServerV2
from c_two.transport.client.core import SharedClient
from c_two.error import (
    ERROR_Code, CCBaseError, CCError,
    CRMDeserializeInput, CRMSerializeOutput, CRMExecuteFunction, CRMServerError,
    CompoSerializeInput, CompoDeserializeOutput, CompoCRMCalling, CompoClientError,
    EventSerializeError, EventDeserializeError,
)
from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello, HelloData

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
    address = f'ipc-v3://error_test_{_next_id()}'
    server = ServerV2(
        bind_address=address,
        icrm_class=IHello,
        crm_instance=ErrorHello(),
        name='default',
    )
    server.start()
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        try:
            if SharedClient.ping(address, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.05)
    yield address
    server.shutdown()


# ---------------------------------------------------------------------------
# Class 1 — CRM execution errors
# ---------------------------------------------------------------------------
class TestCRMExecuteFunction:
    """Errors raised inside a CRM method should surface as CRMExecuteFunction."""

    def test_value_error_in_crm(self, error_server):
        with cc.compo.runtime.connect_crm(error_server, IHello) as crm:
            with pytest.raises(CRMExecuteFunction) as exc_info:
                crm.add(1, 2)
            err = exc_info.value
            assert err.code == ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING
            assert 'division by zero' in str(err)

    def test_type_error_in_crm(self, error_server):
        with cc.compo.runtime.connect_crm(error_server, IHello) as crm:
            with pytest.raises(CRMExecuteFunction) as exc_info:
                crm.greeting('test')
            assert 'name must be str' in str(exc_info.value)

    def test_runtime_error_in_crm(self, error_server):
        with cc.compo.runtime.connect_crm(error_server, IHello) as crm:
            with pytest.raises(CRMExecuteFunction) as exc_info:
                crm.get_data(1)
            assert 'database unavailable' in str(exc_info.value)

    def test_error_is_proper_subclass(self, error_server):
        with cc.compo.runtime.connect_crm(error_server, IHello) as crm:
            with pytest.raises(CRMExecuteFunction) as exc_info:
                crm.add(1, 2)
            err = exc_info.value
            assert isinstance(err, CRMExecuteFunction)
            assert isinstance(err, CCError)
            assert isinstance(err, CCBaseError)
            assert isinstance(err, Exception)

    def test_no_multi_layer_wrapping(self, error_server):
        """Server-side CRM errors must NOT be re-wrapped into a Compo error."""
        with cc.compo.runtime.connect_crm(error_server, IHello) as crm:
            with pytest.raises(CRMExecuteFunction) as exc_info:
                crm.add(1, 2)
            err_str = str(exc_info.value)
            assert 'ERROR_AT_COMPO' not in err_str

    def test_error_propagates_through_connect_crm(self, error_server):
        """connect_crm must NOT swallow exceptions — they must reach the caller."""
        with pytest.raises(CCBaseError):
            with cc.compo.runtime.connect_crm(error_server, IHello) as crm:
                crm.add(1, 2)


# ---------------------------------------------------------------------------
# Class 2 — Compo client errors
# ---------------------------------------------------------------------------
class TestCompoClientError:
    """Errors triggered by client connectivity problems."""

    def test_connect_to_nonexistent_server(self):
        address = f'ipc-v3://nonexistent_{_next_id()}'
        with pytest.raises(Exception):
            with cc.compo.runtime.connect_crm(address, IHello) as crm:
                crm.greeting('test')

    def test_ping_nonexistent_returns_false(self):
        address = f'ipc-v3://nonexistent_{_next_id()}'
        assert SharedClient.ping(address, timeout=0.5) is False


# ---------------------------------------------------------------------------
# Class 3 — Error serialization round-trips
# ---------------------------------------------------------------------------
ALL_SUBCLASSES = [
    CRMDeserializeInput, CRMSerializeOutput, CRMExecuteFunction, CRMServerError,
    CompoSerializeInput, CompoDeserializeOutput, CompoCRMCalling, CompoClientError,
    EventSerializeError, EventDeserializeError,
]


class TestErrorDeserialization:
    """Verify serialize → deserialize returns the proper subclass."""

    def test_crm_execute_function_round_trip(self):
        original = CRMExecuteFunction('test msg')
        data = CCError.serialize(original)
        restored = CCError.deserialize(memoryview(data))
        assert isinstance(restored, CRMExecuteFunction)
        assert restored.code == ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING
        assert 'test msg' in restored.message

    def test_crm_deserialize_input_round_trip(self):
        original = CRMDeserializeInput('bad payload')
        data = CCError.serialize(original)
        restored = CCError.deserialize(memoryview(data))
        assert isinstance(restored, CRMDeserializeInput)
        assert restored.code == ERROR_Code.ERROR_AT_CRM_INPUT_DESERIALIZING
        assert 'bad payload' in restored.message

    @pytest.mark.parametrize('subclass', ALL_SUBCLASSES, ids=lambda c: c.__name__)
    def test_all_error_types_round_trip(self, subclass):
        original = subclass('round-trip detail')
        data = CCError.serialize(original)
        restored = CCError.deserialize(memoryview(data))
        assert isinstance(restored, subclass)
        assert restored.code == original.code
        assert 'round-trip detail' in restored.message

    def test_unknown_code_returns_ccerror(self):
        """An unrecognised numeric code should deserialize as plain CCError."""
        raw = b'0:mysterious failure'
        restored = CCError.deserialize(memoryview(raw))
        assert type(restored) is CCError
        assert restored.code == ERROR_Code.ERROR_UNKNOWN
        assert restored.message == 'mysterious failure'


# ---------------------------------------------------------------------------
# Class 4 — Error constructor contracts
# ---------------------------------------------------------------------------
class TestErrorConstructors:
    """Validate that subclass constructors behave correctly with None / custom messages."""

    @pytest.mark.parametrize('subclass', ALL_SUBCLASSES, ids=lambda c: c.__name__)
    def test_none_message_has_description(self, subclass):
        err = subclass(message=None)
        assert err.message, f'{subclass.__name__} with message=None produced empty message'
        assert len(err.message) > 0

    @pytest.mark.parametrize('subclass', ALL_SUBCLASSES, ids=lambda c: c.__name__)
    def test_message_preserves_detail(self, subclass):
        err = subclass(message='detail')
        assert 'detail' in err.message
        # The description prefix must also be present (not overwritten by detail)
        assert 'Error occurred' in err.message


# ---------------------------------------------------------------------------
# Class 5 — Single-layer propagation (end-to-end)
# ---------------------------------------------------------------------------
class TestSingleLayerPropagation:
    """End-to-end tests ensuring errors propagate as a single layer."""

    def test_server_error_single_layer(self, error_server):
        """Error string should be exactly 2 lines: code+description and original error."""
        with cc.compo.runtime.connect_crm(error_server, IHello) as crm:
            with pytest.raises(CRMExecuteFunction) as exc_info:
                crm.add(1, 2)
            lines = str(exc_info.value).strip().splitlines()
            # Line 1: "ERROR_AT_CRM_FUNCTION_EXECUTING: Error occurred when executing function at CRM:"
            # Line 2: "division by zero"
            assert len(lines) == 2, (
                f'Expected 2-line error (single layer), got {len(lines)} lines:\n'
                + '\n'.join(lines)
            )

    def test_error_code_correct(self, error_server):
        """Error code must be CRM_FUNCTION_EXECUTING, not OUTPUT_SERIALIZING."""
        with cc.compo.runtime.connect_crm(error_server, IHello) as crm:
            with pytest.raises(CRMExecuteFunction) as exc_info:
                crm.add(1, 2)
            assert exc_info.value.code == ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING
            assert exc_info.value.code != ERROR_Code.ERROR_AT_CRM_OUTPUT_SERIALIZING

    def test_original_error_info_preserved(self, error_server):
        """The original exception message must survive serialization round-trip."""
        with cc.compo.runtime.connect_crm(error_server, IHello) as crm:
            with pytest.raises(CRMExecuteFunction) as exc_info:
                crm.greeting('test')
            msg = exc_info.value.message
            assert 'name must be str' in msg
            assert 'executing function at CRM' in msg
