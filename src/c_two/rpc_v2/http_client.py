"""Concurrent HTTP client for CRM access through a relay server.

Provides :class:`HttpClient` — a thread-safe HTTP client backed by
``httpx.Client`` with built-in connection pooling — and
:class:`HttpClientPool` — a reference-counted pool that allows multiple
ICRM consumers to share a single HTTP connection pool to each relay.

The HTTP API uses a simple REST-style convention::

    POST /{route_name}/{method_name}
    Content-Type: application/octet-stream
    Body: serialized args payload (same bytes that ICRM transferable produces)

    → 200  application/octet-stream  result payload
    → 500  application/octet-stream  serialized CCError
    → 502  text/plain               relay/transport error

Usage::

    client = HttpClient('http://localhost:8080')
    result = client.call('hello', data, name='grid')
    client.terminate()

Integrated with :func:`cc.connect`::

    icrm = cc.connect(IGrid, name='grid', address='http://relay:8080')
    icrm.hello('World')
    cc.close(icrm)
"""
from __future__ import annotations

import logging
import threading

import httpx

from .. import error

logger = logging.getLogger(__name__)

_DEFAULT_GRACE_SECONDS = 60.0


class HttpClient:
    """Thread-safe HTTP client for CRM calls through a relay server.

    Uses ``httpx.Client`` internally — supports persistent connections,
    keep-alive, and a configurable connection pool.

    The ``call()`` signature matches :class:`SharedClient` so that
    :class:`ICRMProxy` can use either interchangeably.

    Parameters
    ----------
    base_url:
        Relay server URL (e.g. ``'http://localhost:8080'``).
    timeout:
        Request timeout in seconds.
    max_connections:
        Maximum number of connections in the pool.
    """

    __slots__ = ('_base_url', '_client', '_closed')

    def __init__(
        self,
        base_url: str,
        *,
        timeout: float = 30.0,
        max_connections: int = 100,
    ):
        self._base_url = base_url.rstrip('/')
        self._client = httpx.Client(
            base_url=self._base_url,
            transport=httpx.HTTPTransport(
                limits=httpx.Limits(
                    max_connections=max_connections,
                    max_keepalive_connections=max_connections // 2,
                ),
            ),
            timeout=httpx.Timeout(timeout),
        )
        self._closed = False

    # ------------------------------------------------------------------
    # Public API (same shape as SharedClient)
    # ------------------------------------------------------------------

    def call(
        self,
        method_name: str,
        data: bytes | None = None,
        *,
        name: str | None = None,
    ) -> bytes:
        """Send a CRM call via HTTP.  Thread-safe.

        Parameters
        ----------
        method_name:
            The method to invoke on the CRM.
        data:
            Serialized arguments payload.
        name:
            Target CRM routing name (becomes the URL path prefix).

        Returns
        -------
        bytes
            Serialized result payload.

        Raises
        ------
        CCError
            If the CRM method raised an error (HTTP 500).
        error.CompoClientError
            On transport / relay errors.
        """
        if self._closed:
            raise error.CompoClientError('HttpClient is closed')

        route = name or '_default'
        url = f'/{route}/{method_name}'
        body = data if data is not None else b''

        try:
            resp = self._client.post(
                url,
                content=body,
                headers={'Content-Type': 'application/octet-stream'},
            )
        except httpx.HTTPError as exc:
            raise error.CompoClientError(
                f'HTTP request to {self._base_url}{url} failed: {exc}',
            ) from exc

        if resp.status_code == 200:
            return resp.content

        if resp.status_code == 500:
            # Server returned a serialized CCError.
            try:
                err = error.CCError.deserialize(memoryview(resp.content))
            except Exception:
                err = None
            if err is not None:
                raise err
            raise error.CompoClientError(
                f'CRM error (could not deserialize): '
                f'{resp.content[:200]!r}',
            )

        raise error.CompoClientError(
            f'HTTP {resp.status_code} from relay: '
            f'{resp.text[:200]}',
        )

    def health(self) -> dict:
        """Query the relay's health endpoint.

        Returns
        -------
        dict
            JSON response from ``GET /health``.
        """
        resp = self._client.get('/health')
        resp.raise_for_status()
        return resp.json()

    def terminate(self) -> None:
        """Close the underlying HTTP connection pool."""
        if not self._closed:
            self._closed = True
            self._client.close()

    # ------------------------------------------------------------------
    # Diagnostics
    # ------------------------------------------------------------------

    @staticmethod
    def ping(base_url: str, *, timeout: float = 2.0) -> bool:
        """Quick health-check against a relay server."""
        try:
            resp = httpx.get(f'{base_url.rstrip("/")}/health', timeout=timeout)
            return resp.status_code == 200
        except httpx.HTTPError:
            return False

    def __repr__(self) -> str:
        state = 'closed' if self._closed else 'open'
        return f'<HttpClient {self._base_url!r} ({state})>'


# ======================================================================
# HttpClientPool — reference-counted pool (mirrors ClientPool)
# ======================================================================


class HttpClientPool:
    """Reference-counted pool of :class:`HttpClient` instances.

    Ensures that multiple ICRM consumers connecting to the same relay
    share a single ``httpx.Client`` (and thus a single TCP connection
    pool).  Follows the same acquire / release / grace-period pattern as
    :class:`ClientPool`.

    Parameters
    ----------
    grace_seconds:
        Seconds to wait after the last release before destroying the
        client.
    """

    def __init__(self, grace_seconds: float = _DEFAULT_GRACE_SECONDS):
        self._grace_seconds = grace_seconds
        self._lock = threading.Lock()
        self._clients: dict[str, HttpClient] = {}
        self._refcounts: dict[str, int] = {}
        self._timers: dict[str, threading.Timer] = {}

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def acquire(self, base_url: str) -> HttpClient:
        """Get (or create) an :class:`HttpClient` for *base_url*.

        Increments the reference count.  Caller **must** call
        :meth:`release` when done.
        """
        with self._lock:
            timer = self._timers.pop(base_url, None)
            if timer is not None:
                timer.cancel()

            client = self._clients.get(base_url)
            if client is not None:
                self._refcounts[base_url] += 1
                return client

            client = HttpClient(base_url)
            self._clients[base_url] = client
            self._refcounts[base_url] = 1
            return client

    def release(self, base_url: str) -> None:
        """Decrement reference count; schedule grace-period destroy at 0."""
        with self._lock:
            count = self._refcounts.get(base_url, 0)
            if count <= 0:
                logger.warning(
                    'HttpClientPool.release() with no matching acquire for %s',
                    base_url,
                )
                return
            count -= 1
            self._refcounts[base_url] = count

            if count == 0:
                timer = threading.Timer(
                    self._grace_seconds,
                    self._destroy,
                    args=(base_url,),
                )
                timer.daemon = True
                timer.name = f'c2-http-pool-grace-{base_url}'
                self._timers[base_url] = timer
                timer.start()

    def shutdown_all(self) -> None:
        """Terminate all clients immediately."""
        with self._lock:
            for timer in self._timers.values():
                timer.cancel()
            self._timers.clear()
            clients = list(self._clients.values())
            self._clients.clear()
            self._refcounts.clear()

        for client in clients:
            try:
                client.terminate()
            except Exception:
                logger.warning('Failed to terminate HttpClient', exc_info=True)

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _destroy(self, base_url: str) -> None:
        """Called by grace timer after all references released."""
        with self._lock:
            self._timers.pop(base_url, None)
            if self._refcounts.get(base_url, 0) > 0:
                return
            client = self._clients.pop(base_url, None)
            self._refcounts.pop(base_url, None)

        if client is not None:
            logger.debug(
                'Destroying HttpClient for %s (grace period expired)',
                base_url,
            )
            try:
                client.terminate()
            except Exception:
                logger.warning(
                    'Failed to terminate HttpClient %s',
                    base_url,
                    exc_info=True,
                )

    # ------------------------------------------------------------------
    # Introspection (for testing)
    # ------------------------------------------------------------------

    def active_count(self) -> int:
        with self._lock:
            return len(self._clients)

    def refcount(self, base_url: str) -> int:
        with self._lock:
            return self._refcounts.get(base_url, 0)

    def has_client(self, base_url: str) -> bool:
        with self._lock:
            return base_url in self._clients
