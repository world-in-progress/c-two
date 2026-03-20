import os
import pytest
import threading
import time
import c_two as cc

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
def unique_thread_address():
    return f'thread://test_hello_{_next_id()}'


@pytest.fixture
def unique_memory_address():
    return f'memory://test_hello_{_next_id()}'


@pytest.fixture
def unique_tcp_address():
    port = 15000 + _next_id()
    return f'tcp://127.0.0.1:{port}'


@pytest.fixture
def unique_http_address():
    port = 16000 + _next_id()
    return f'http://127.0.0.1:{port}'


@pytest.fixture
def unique_ipc_v2_address():
    return f'ipc-v2://test_hello_{_next_id()}'


@pytest.fixture(params=['thread', 'memory', 'tcp', 'http', 'ipc-v2'])
def protocol_address(request, unique_thread_address, unique_memory_address, unique_tcp_address, unique_http_address, unique_ipc_v2_address):
    """Parametrized fixture that provides a unique address for each protocol."""
    addresses = {
        'thread': unique_thread_address,
        'memory': unique_memory_address,
        'tcp': unique_tcp_address,
        'http': unique_http_address,
        'ipc-v2': unique_ipc_v2_address,
    }
    return addresses[request.param]


@pytest.fixture
def hello_crm():
    """Create a Hello CRM instance."""
    return Hello()


@pytest.fixture
def hello_server(protocol_address, hello_crm):
    """Start a Hello CRM server on the given protocol address, yield the address, then shut down."""
    config = cc.rpc.ServerConfig(
        name='TestHello',
        crm=hello_crm,
        icrm=IHello,
        bind_address=protocol_address,
    )

    server = cc.rpc.Server(config)

    # Start server in background thread (don't call server.start() because it blocks)
    from c_two.rpc.server import _start
    _start(server._state)

    # Wait for server to be ready
    retries = 50
    for _ in range(retries):
        try:
            if cc.rpc.Client.ping(protocol_address, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.1)

    yield protocol_address

    # Cleanup
    try:
        cc.rpc.Client.shutdown(protocol_address, timeout=1.0)
    except Exception:
        pass
    time.sleep(0.1)
    try:
        server.stop()
    except Exception:
        pass
