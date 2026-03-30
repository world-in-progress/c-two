import os

# Disable .env loading before any c_two import — must be first.
os.environ['C2_ENV_FILE'] = ''

import pytest
import threading
import time
import c_two as cc

from c_two.transport.server.core import Server
from c_two.transport.client.core import SharedClient

from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello

# Disable proxy for localhost to avoid HTTP test failures
os.environ.setdefault('NO_PROXY', '127.0.0.1,localhost')
os.environ.setdefault('no_proxy', '127.0.0.1,localhost')


# Unique address factory to avoid conflicts between tests
_address_counter = 0
_address_lock = threading.Lock()

def _next_id():
    global _address_counter
    with _address_lock:
        _address_counter += 1
        return _address_counter


@pytest.fixture
def unique_ipc_address():
    return f'ipc://test_hello_{_next_id()}'


@pytest.fixture(params=['ipc'])
def protocol_address(request, unique_ipc_address):
    """Parametrized fixture providing a unique address for each supported protocol."""
    addresses = {
        'ipc': unique_ipc_address,
    }
    return addresses[request.param]


@pytest.fixture
def hello_crm():
    """Create a Hello CRM instance."""
    return Hello()


def _wait_for_server(address: str, timeout: float = 5.0) -> None:
    """Poll until the server responds to ping."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if SharedClient.ping(address, timeout=0.5):
                return
        except Exception:
            pass
        time.sleep(0.05)
    raise TimeoutError(f'Server at {address} not ready after {timeout}s')


@pytest.fixture
def hello_server(protocol_address, hello_crm):
    """Start a Hello CRM server on the given protocol, yield the address, then shut down."""
    server = Server(
        bind_address=protocol_address,
        icrm_class=IHello,
        crm_instance=hello_crm,
    )
    server.start()
    _wait_for_server(protocol_address)

    yield protocol_address

    server.shutdown()
