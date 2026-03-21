import threading
import time

import pytest

import c_two as cc
from c_two.rpc import (
    ConcurrencyConfig,
    ConcurrencyMode,
    Router,
    RouterConfig,
    ServerConfig,
    WorkerHandle,
)
from c_two.rpc.server import _start
from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello

pytestmark = pytest.mark.timeout(30)

_counter = 0
_counter_lock = threading.Lock()


def _next_id() -> int:
    global _counter
    with _counter_lock:
        _counter += 1
        return _counter


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def router_with_worker():
    """Spin up a Router + a co-located thread:// Worker and yield (router_url, worker_address)."""
    worker_address = f'thread://router_test_worker_{_next_id()}'
    router_port = 18000 + _next_id()
    router_url = f'http://127.0.0.1:{router_port}'

    crm = Hello()
    server = cc.rpc.Server(ServerConfig(
        name='RouterTestHello',
        crm=crm,
        icrm=IHello,
        bind_address=worker_address,
    ))
    _start(server._state)

    for _ in range(50):
        try:
            if cc.rpc.Client.ping(worker_address, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.1)

    router = Router(RouterConfig(bind_address=router_url))
    router.attach(server)
    router.start(blocking=False)
    time.sleep(0.5)

    yield router_url, worker_address, router, server

    router.stop()
    try:
        cc.rpc.Client.shutdown(worker_address, timeout=2.0)
    except Exception:
        pass
    time.sleep(0.1)
    try:
        server.stop()
    except Exception:
        pass


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


class TestRouterRegistration:
    def test_register_and_unregister(self):
        router = Router(RouterConfig(bind_address='http://127.0.0.1:0'))

        router.register('cc.demo', 'thread://worker_a')
        assert 'cc.demo' in router.workers

        router.unregister('cc.demo')
        assert 'cc.demo' not in router.workers

    def test_attach_reads_icrm_tag(self):
        address = f'thread://router_attach_test_{_next_id()}'
        server = cc.rpc.Server(ServerConfig(
            crm=Hello(),
            icrm=IHello,
            bind_address=address,
        ))

        router = Router(RouterConfig(bind_address='http://127.0.0.1:0'))
        router.attach(server)

        tag: str = getattr(server._state.icrm, '__tag__', '')
        expected_namespace = tag.split('/')[0]

        assert expected_namespace in router.workers
        assert router.workers[expected_namespace].address == address


class TestRouterRelayEndToEnd:
    def test_ping_through_router(self, router_with_worker):
        router_url, _, _, _ = router_with_worker
        tag: str = IHello.__tag__
        namespace = tag.split('/')[0]

        result = cc.rpc.Client.ping(f'{router_url}/{namespace}', timeout=5.0)
        assert result is True

    def test_crm_call_through_router(self, router_with_worker):
        router_url, _, _, _ = router_with_worker
        tag: str = IHello.__tag__
        namespace = tag.split('/')[0]
        endpoint = f'{router_url}/{namespace}'

        with cc.compo.runtime.connect_crm(endpoint, IHello) as crm:
            result = crm.greeting('RouterWorld')

        assert result == 'Hello, RouterWorld!'

    def test_multiple_calls_through_router(self, router_with_worker):
        router_url, _, _, _ = router_with_worker
        tag: str = IHello.__tag__
        namespace = tag.split('/')[0]
        endpoint = f'{router_url}/{namespace}'

        with cc.compo.runtime.connect_crm(endpoint, IHello) as crm:
            r1 = crm.greeting('A')
            r2 = crm.greeting('B')
            r3 = crm.add(10, 20)

        assert r1 == 'Hello, A!'
        assert r2 == 'Hello, B!'
        assert r3 == 30

    def test_unknown_namespace_returns_error(self, router_with_worker):
        import requests as http_requests

        router_url, _, _, _ = router_with_worker
        from c_two.rpc.event import Event, EventTag
        event_bytes = Event(tag=EventTag.PING).serialize()
        resp = http_requests.post(
            f'{router_url}/nonexistent.namespace',
            data=event_bytes,
            timeout=5.0,
        )
        assert resp.status_code == 404


class TestRouterHealthEndpoint:
    def test_health_returns_ok(self, router_with_worker):
        import requests as http_requests

        router_url, _, _, _ = router_with_worker
        resp = http_requests.get(f'{router_url}/health', timeout=5.0)
        assert resp.status_code == 200
        body = resp.json()
        assert body['status'] == 'ok'
        assert isinstance(body['workers'], list)
        assert len(body['workers']) >= 1
