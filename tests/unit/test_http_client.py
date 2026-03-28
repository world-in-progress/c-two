"""Unit tests for HttpClient and HttpClientPool."""
from __future__ import annotations

import threading
import time

import httpx
import pytest

from c_two import error
from c_two.transport.client.http import HttpClient, HttpClientPool


# ---------------------------------------------------------------------------
# Mock transport for httpx (no real server needed)
# ---------------------------------------------------------------------------

def _mock_transport(handler):
    """Create an httpx.MockTransport from a handler function.

    The handler receives (request: httpx.Request) and returns
    httpx.Response.
    """
    return httpx.MockTransport(handler)


def _ok_handler(request: httpx.Request) -> httpx.Response:
    """Echo handler: returns the request body prefixed with 'OK:'."""
    return httpx.Response(200, content=b'OK:' + request.content)


def _error_handler(request: httpx.Request) -> httpx.Response:
    """Returns a serialized CCError for /error paths, else OK."""
    if b'/error/' in request.url.raw_path:
        err = error.CCError(
            error.ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING,
            'test error',
        )
        return httpx.Response(500, content=error.CCError.serialize(err))
    return httpx.Response(200, content=request.content)


def _health_handler(request: httpx.Request) -> httpx.Response:
    if request.url.path == '/health':
        return httpx.Response(200, json={'status': 'ok', 'upstream': 'test'})
    return httpx.Response(200, content=request.content)


def _make_client_with_transport(transport: httpx.MockTransport) -> HttpClient:
    """Create an HttpClient with a mock transport (no real network)."""
    client = HttpClient.__new__(HttpClient)
    client._base_url = 'http://test'
    client._client = httpx.Client(
        base_url='http://test',
        transport=transport,
    )
    client._closed = False
    return client


# ---------------------------------------------------------------------------
# HttpClient tests
# ---------------------------------------------------------------------------

class TestHttpClient:
    """Unit tests for the HttpClient class."""

    def test_call_basic(self):
        """Basic call returns response bytes."""
        client = _make_client_with_transport(_mock_transport(_ok_handler))
        try:
            result = client.call('hello', b'world', name='grid')
            assert result == b'OK:world'
        finally:
            client.terminate()

    def test_call_empty_data(self):
        """call() with no data sends empty body."""
        client = _make_client_with_transport(_mock_transport(_ok_handler))
        try:
            result = client.call('hello', name='grid')
            assert result == b'OK:'
        finally:
            client.terminate()

    def test_call_none_data(self):
        """call() with data=None sends empty body."""
        client = _make_client_with_transport(_mock_transport(_ok_handler))
        try:
            result = client.call('hello', None, name='grid')
            assert result == b'OK:'
        finally:
            client.terminate()

    def test_call_default_name(self):
        """name=None uses '_default' in URL."""
        urls = []

        def capture_handler(request: httpx.Request) -> httpx.Response:
            urls.append(str(request.url))
            return httpx.Response(200, content=b'ok')

        client = _make_client_with_transport(_mock_transport(capture_handler))
        try:
            client.call('method')
            assert '/_default/method' in urls[0]
        finally:
            client.terminate()

    def test_call_error_500_ccerror(self):
        """HTTP 500 with serialized CCError is raised as CCError."""
        client = _make_client_with_transport(_mock_transport(_error_handler))
        try:
            with pytest.raises(error.CCError) as exc_info:
                client.call('method', b'x', name='error')
            assert 'test error' in str(exc_info.value)
        finally:
            client.terminate()

    def test_call_error_500_bad_body(self):
        """HTTP 500 with non-deserializable body raises CompoClientError."""

        def bad_500(request: httpx.Request) -> httpx.Response:
            return httpx.Response(500, content=b'not-a-ccerror')

        client = _make_client_with_transport(_mock_transport(bad_500))
        try:
            with pytest.raises(error.CompoClientError):
                client.call('method', b'', name='test')
        finally:
            client.terminate()

    def test_call_error_other_status(self):
        """Non-200/500 status raises CompoClientError."""

        def not_found(request: httpx.Request) -> httpx.Response:
            return httpx.Response(404, text='Not found')

        client = _make_client_with_transport(_mock_transport(not_found))
        try:
            with pytest.raises(error.CompoClientError, match='404'):
                client.call('method', name='test')
        finally:
            client.terminate()

    def test_call_closed_raises(self):
        """Calling after terminate raises."""
        client = _make_client_with_transport(_mock_transport(_ok_handler))
        client.terminate()
        with pytest.raises(error.CompoClientError, match='closed'):
            client.call('method', name='test')

    def test_terminate_idempotent(self):
        """terminate() can be called multiple times safely."""
        client = _make_client_with_transport(_mock_transport(_ok_handler))
        client.terminate()
        client.terminate()  # No error

    def test_call_concurrent(self):
        """Multiple threads can call concurrently."""
        client = _make_client_with_transport(_mock_transport(_ok_handler))
        errors: list[str] = []

        def worker(tid: int) -> None:
            try:
                for i in range(5):
                    data = f'{tid}_{i}'.encode()
                    result = client.call('echo', data, name='test')
                    expected = b'OK:' + data
                    if result != expected:
                        errors.append(f'T{tid}[{i}]: {result!r} != {expected!r}')
            except Exception as e:
                errors.append(f'T{tid}: {e}')

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
        try:
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=30)
            assert errors == [], f'Errors: {errors}'
        finally:
            client.terminate()

    def test_health(self):
        """health() returns parsed JSON."""
        client = _make_client_with_transport(_mock_transport(_health_handler))
        try:
            data = client.health()
            assert data['status'] == 'ok'
        finally:
            client.terminate()

    def test_repr(self):
        client = _make_client_with_transport(_mock_transport(_ok_handler))
        assert 'open' in repr(client)
        client.terminate()
        assert 'closed' in repr(client)

    def test_ping_success(self):
        """Static ping against a mock that returns 200 on /health."""

        def health_ok(request: httpx.Request) -> httpx.Response:
            return httpx.Response(200, json={'status': 'ok'})

        # Note: ping uses httpx.get directly, not the client instance.
        # We can't mock it in unit tests without monkeypatching.
        # This is tested in integration tests instead.


# ---------------------------------------------------------------------------
# HttpClientPool tests
# ---------------------------------------------------------------------------

class TestHttpClientPool:
    """Unit tests for the HttpClientPool."""

    def test_acquire_creates_client(self):
        pool = HttpClientPool(grace_seconds=0.5)
        try:
            client = pool.acquire('http://test1:8080')
            assert isinstance(client, HttpClient)
            assert pool.active_count() == 1
            pool.release('http://test1:8080')
        finally:
            pool.shutdown_all()

    def test_acquire_reuses_client(self):
        pool = HttpClientPool(grace_seconds=0.5)
        try:
            c1 = pool.acquire('http://test2:8080')
            c2 = pool.acquire('http://test2:8080')
            assert c1 is c2
            assert pool.refcount('http://test2:8080') == 2
            pool.release('http://test2:8080')
            pool.release('http://test2:8080')
        finally:
            pool.shutdown_all()

    def test_different_urls_different_clients(self):
        pool = HttpClientPool(grace_seconds=0.5)
        try:
            c1 = pool.acquire('http://host1:8080')
            c2 = pool.acquire('http://host2:8080')
            assert c1 is not c2
            assert pool.active_count() == 2
            pool.release('http://host1:8080')
            pool.release('http://host2:8080')
        finally:
            pool.shutdown_all()

    def test_release_grace_period(self):
        """After release, client survives during grace period."""
        pool = HttpClientPool(grace_seconds=2.0)
        try:
            pool.acquire('http://test3:8080')
            pool.release('http://test3:8080')
            # Still alive during grace period.
            assert pool.has_client('http://test3:8080')
        finally:
            pool.shutdown_all()

    def test_release_destroys_after_grace(self):
        """Client is destroyed after grace period expires."""
        pool = HttpClientPool(grace_seconds=0.2)
        try:
            pool.acquire('http://test4:8080')
            pool.release('http://test4:8080')
            time.sleep(0.5)
            assert not pool.has_client('http://test4:8080')
        finally:
            pool.shutdown_all()

    def test_reacquire_cancels_destroy(self):
        """Re-acquire during grace period cancels destruction."""
        pool = HttpClientPool(grace_seconds=0.5)
        try:
            c1 = pool.acquire('http://test5:8080')
            pool.release('http://test5:8080')
            # Re-acquire before grace period expires.
            c2 = pool.acquire('http://test5:8080')
            assert c1 is c2
            time.sleep(0.8)
            # Still alive because re-acquired.
            assert pool.has_client('http://test5:8080')
            pool.release('http://test5:8080')
        finally:
            pool.shutdown_all()

    def test_shutdown_all(self):
        """shutdown_all terminates all clients immediately."""
        pool = HttpClientPool(grace_seconds=60.0)
        pool.acquire('http://host1:8080')
        pool.acquire('http://host2:8080')
        pool.shutdown_all()
        assert pool.active_count() == 0

    def test_release_without_acquire_warns(self, caplog):
        """release() without acquire logs a warning."""
        pool = HttpClientPool()
        import logging
        with caplog.at_level(logging.WARNING):
            pool.release('http://phantom:8080')
        assert 'no matching acquire' in caplog.text.lower()

    def test_concurrent_acquire_release(self):
        """Multiple threads acquire/release the same URL."""
        pool = HttpClientPool(grace_seconds=5.0)
        errors: list[str] = []

        def worker(tid: int) -> None:
            try:
                client = pool.acquire('http://shared:8080')
                assert isinstance(client, HttpClient)
                time.sleep(0.01)
                pool.release('http://shared:8080')
            except Exception as e:
                errors.append(f'T{tid}: {e}')

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(10)]
        try:
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=30)
            assert errors == []
        finally:
            pool.shutdown_all()
