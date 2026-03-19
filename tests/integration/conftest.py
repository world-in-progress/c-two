import time
import pytest
import threading

import c_two as cc
from c_two.rpc.server import _start
from examples.grid.crm import Grid
from examples.grid.icrm import IGrid


_grid_counter = 0
_grid_lock = threading.Lock()

def _next_grid_id():
    global _grid_counter
    with _grid_lock:
        _grid_counter += 1
        return _grid_counter


@pytest.fixture
def grid_crm():
    """Create a Grid CRM with small test parameters."""
    return Grid(
        epsg=2326,
        bounds=[808357.5, 824117.5, 838949.5, 843957.5],
        first_size=[64.0, 64.0],
        subdivide_rules=[[478, 310], [2, 2], [2, 2], [2, 2], [2, 2], [2, 2], [1, 1]],
    )


@pytest.fixture
def grid_server(grid_crm):
    """Start a Grid CRM server on a thread address, yield the address, then shut down."""
    address = f'thread://grid_test_{_next_grid_id()}'

    config = cc.rpc.ServerConfig(
        name='TestGrid',
        crm=grid_crm,
        icrm=IGrid,
        bind_address=address,
        on_shutdown=grid_crm.terminate,
    )

    server = cc.rpc.Server(config)
    _start(server._state)

    # Wait for server to be ready
    for _ in range(50):
        try:
            if cc.rpc.Client.ping(address, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.1)

    yield address

    try:
        cc.rpc.Client.shutdown(address, timeout=2.0)
    except Exception:
        pass
    time.sleep(0.1)
    try:
        server.stop()
    except Exception:
        pass
